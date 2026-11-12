//! The central event bus (issue #8, ADR-013).
//!
//! One bus per application: created in `setup` and held by `AppState`. It
//! only delivers notifications. It carries no requests, replies or commands,
//! spawns no tasks, has no access to the module manager or to modules (it
//! knows only the event types) and therefore never changes module statuses
//! or starts enable, disable, recover, restart or any automatic recovery.
//!
//! Delivery guarantees:
//! - `publish`, `subscribe` and `close` each take effect under one bus lock,
//!   so they happen one after another and never interleave;
//! - `publish` assigns `seq` and enqueues the event under that lock, so the
//!   `seq` order is the delivery order for every subscriber, also when several
//!   threads publish at once; events published by one thread keep its order;
//! - a subscriber receives only the events published after it subscribed;
//!   earlier events are not replayed;
//! - the buffer keeps `capacity` events for each subscriber. `publish` never
//!   waits: a subscriber that falls further behind receives
//!   `Lagged { missed }` with the exact number of events dropped for it and
//!   then continues with the oldest event still buffered. Publishers and other
//!   subscribers are not affected;
//! - after `close` (or when the last handle is dropped) subscribers receive
//!   every event published before it, then `Closed`.
//!
//! Requirement for callers: a waker used to poll [`Subscriber::recv`] must not
//! call the bus (`publish`, `subscribe` or `close`) synchronously. Subscribers
//! are woken while the bus lock is held, so such a call would deadlock. Wakers
//! of tokio and other common executors only schedule the task and meet this.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use thiserror::Error;
use tokio::sync::broadcast;
use tokio::sync::broadcast::error::RecvError;

use super::types::{AppEvent, EventEnvelope, EventSource};
use crate::clock::unix_time_ms;

/// Capacity used by the application: events kept for each subscriber.
pub const DEFAULT_CAPACITY: usize = 256;

/// Largest accepted capacity. The whole buffer is allocated when the bus is
/// created, so the limit keeps a wrong constant from reserving a lot of memory.
pub const MAX_CAPACITY: usize = 4096;

// `setup` creates the bus with `DEFAULT_CAPACITY`; it must never panic there.
const _: () = assert!(DEFAULT_CAPACITY.is_power_of_two() && DEFAULT_CAPACITY <= MAX_CAPACITY);

/// What [`Subscriber::recv`] returns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Received {
    /// The next event, in `seq` order.
    Event(EventEnvelope),
    /// The subscriber fell more than the capacity behind and the `missed`
    /// oldest events were dropped for it. It must resynchronise from the
    /// status snapshot (#9). Delivery continues with the oldest event still
    /// buffered, or with another `Lagged` if the subscriber is still too slow.
    Lagged { missed: u64 },
    /// The bus is closed and every event published before closing has been
    /// received. Every later call returns `Closed` as well.
    Closed,
}

/// Returned by [`EventBus::publish`] after the bus has been closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[error("the event bus is closed")]
pub struct BusClosed;

/// Handle to the central bus. Clones share one channel and one `seq` counter.
#[derive(Debug, Clone)]
pub struct EventBus {
    shared: Arc<Mutex<Shared>>,
}

#[derive(Debug)]
struct Shared {
    /// `None` once the bus is closed.
    sender: Option<broadcast::Sender<EventEnvelope>>,
    /// `seq` of the last accepted event; 0 before the first one.
    last_seq: u64,
}

impl EventBus {
    /// Creates a bus that keeps up to `capacity` events for each subscriber.
    ///
    /// # Panics
    ///
    /// If `capacity` is not a power of two from 1 to [`MAX_CAPACITY`]. The
    /// capacity is a constant in the code, not a user setting. `tokio` rounds
    /// a capacity up to a power of two; accepting only powers of two keeps the
    /// configured value exactly the point where `Lagged` starts.
    pub fn new(capacity: usize) -> Self {
        assert!(
            capacity.is_power_of_two() && capacity <= MAX_CAPACITY,
            "event bus capacity must be a power of two from 1 to {MAX_CAPACITY}, got {capacity}"
        );
        Self {
            shared: Arc::new(Mutex::new(Shared {
                // No receiver is created together with the sender, so events
                // published while nobody is subscribed are not buffered.
                sender: Some(broadcast::Sender::new(capacity)),
                last_seq: 0,
            })),
        }
    }

    /// Publishes `event` with [`EventSource::Core`] and returns its `seq`.
    ///
    /// Never waits for subscribers. Without subscribers the event is
    /// delivered to nobody, which is not an error. The caller cannot choose
    /// the source.
    ///
    /// Waiting subscribers are woken during this call while the bus lock is
    /// held: their wakers must not call the bus (see the module requirement).
    ///
    /// # Errors
    ///
    /// [`BusClosed`] after [`close`](Self::close); no `seq` is used then.
    pub fn publish(&self, event: AppEvent) -> Result<u64, BusClosed> {
        let mut shared = self.lock();
        let Shared { sender, last_seq } = &mut *shared;
        let sender = sender.as_ref().ok_or(BusClosed)?;
        *last_seq += 1;
        let envelope = EventEnvelope {
            seq: *last_seq,
            at_unix_ms: unix_time_ms(),
            source: EventSource::Core,
            event,
        };
        // The event has its `seq` but is not sent yet: tests stop a publisher
        // here to check that nothing can overtake it.
        #[cfg(test)]
        test_hooks::before_send(envelope.seq);
        // Sending under the lock makes the `seq` order the delivery order.
        // `send` fails only when nobody is subscribed: the event is then
        // delivered to nobody, which is fine for a notification.
        let _ = sender.send(envelope);
        Ok(*last_seq)
    }

    /// Subscribes to the events published after this call; earlier events
    /// are not replayed. To follow module statuses, subscribe first and then
    /// read the status snapshot (#9). After [`close`](Self::close) the
    /// subscriber receives [`Received::Closed`] at once.
    pub fn subscribe(&self) -> Subscriber {
        let receiver = self
            .lock()
            .sender
            .as_ref()
            .map(broadcast::Sender::subscribe);
        Subscriber { receiver }
    }

    /// Closes the bus for every handle: subscribers receive the events
    /// published before, then [`Received::Closed`]; later `publish` calls fail
    /// with [`BusClosed`]. Calling it again does nothing.
    pub fn close(&self) {
        let closed = {
            let mut shared = self.lock();
            let last_seq = shared.last_seq;
            shared
                .sender
                .take()
                .map(|sender| (last_seq, sender.receiver_count()))
        };
        if let Some((last_seq, subscribers)) = closed {
            tracing::info!(last_seq, subscribers, "event bus closed");
        }
    }

    /// Each critical section is a single send, subscribe or close, so the
    /// state stays consistent even if a lock holder panicked: a poisoned lock
    /// is recovered instead of spreading the panic.
    fn lock(&self) -> MutexGuard<'_, Shared> {
        #[cfg(test)]
        test_hooks::before_lock(&self.shared);
        self.shared.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Receiving side of the bus for one consumer. Dropping it unsubscribes.
#[derive(Debug)]
pub struct Subscriber {
    /// `None` when subscribed after the bus was closed.
    receiver: Option<broadcast::Receiver<EventEnvelope>>,
}

impl Subscriber {
    /// Waits for the next event, a lag report or the end of the bus.
    ///
    /// Cancel safe: if the future is dropped before it completes, no event is
    /// lost. The waker of the task polling this future is called while the
    /// bus lock is held, so it must not call the bus synchronously (see the
    /// module requirement).
    pub async fn recv(&mut self) -> Received {
        let Some(receiver) = self.receiver.as_mut() else {
            return Received::Closed;
        };
        match receiver.recv().await {
            Ok(envelope) => Received::Event(envelope),
            Err(RecvError::Lagged(missed)) => Received::Lagged { missed },
            Err(RecvError::Closed) => Received::Closed,
        }
    }
}

/// Interception points that let a test force a particular interleaving of
/// threads instead of waiting for a race to happen. Compiled only into the
/// test build: the application has no hooks. A hook is set per thread, so
/// tests running in parallel do not affect each other.
#[cfg(test)]
mod test_hooks {
    use std::cell::RefCell;
    use std::sync::{Mutex, TryLockError};

    type BeforeSend = Option<Box<dyn FnMut(u64)>>;
    type BeforeWait = Option<Box<dyn FnMut()>>;

    thread_local! {
        /// Called by `publish` on this thread once the event has its `seq`,
        /// just before it is sent; the bus lock is held at that moment.
        pub(super) static BEFORE_SEND: RefCell<BeforeSend> = const { RefCell::new(None) };
        /// Called on this thread when it is about to wait for the bus lock
        /// because another thread holds it.
        pub(super) static BEFORE_WAIT: RefCell<BeforeWait> = const { RefCell::new(None) };
    }

    pub(super) fn before_send(seq: u64) {
        BEFORE_SEND.with_borrow_mut(|hook| {
            if let Some(hook) = hook {
                hook(seq);
            }
        });
    }

    pub(super) fn before_lock<T>(lock: &Mutex<T>) {
        BEFORE_WAIT.with_borrow_mut(|hook| {
            if let Some(hook) = hook
                && let Err(TryLockError::WouldBlock) = lock.try_lock()
            {
                hook();
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use std::pin::pin;
    use std::sync::{Barrier, mpsc};
    use std::task::{Context, Poll, Waker};
    use std::thread::{self, JoinHandle};

    use super::test_hooks::{BEFORE_SEND, BEFORE_WAIT};
    use super::*;
    use crate::events::types::ModuleEvent;
    use crate::modules::contract::{FailurePhase, ModuleId, ModuleState, ModuleStatus};

    /// Runs a future to completion on a current-thread tokio runtime. Used
    /// only by threads that must wait for other threads.
    fn block_on<F: Future>(future: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("test runtime")
            .block_on(future)
    }

    /// Polls a future once without a runtime: `Pending` means nothing is
    /// ready yet.
    fn poll_once<F: Future>(future: F) -> Poll<F::Output> {
        pin!(future).poll(&mut Context::from_waker(Waker::noop()))
    }

    /// The message that must already be waiting for the subscriber. A
    /// regression makes the test fail here instead of hanging.
    fn ready(subscriber: &mut Subscriber) -> Received {
        match poll_once(subscriber.recv()) {
            Poll::Ready(received) => received,
            Poll::Pending => panic!("expected a message to be ready"),
        }
    }

    fn id(value: &str) -> ModuleId {
        ModuleId::new(value).expect("valid module id")
    }

    /// A status change of `module`: enabled, `Stopped` -> `Running`.
    fn status_changed(module: &str) -> AppEvent {
        AppEvent::Module(ModuleEvent::StatusChanged {
            id: id(module),
            previous: ModuleStatus {
                enabled: true,
                state: ModuleState::Stopped,
            },
            current: ModuleStatus {
                enabled: true,
                state: ModuleState::Running,
            },
        })
    }

    fn next_event(subscriber: &mut Subscriber) -> EventEnvelope {
        match ready(subscriber) {
            Received::Event(envelope) => envelope,
            other => panic!("expected an event, got {other:?}"),
        }
    }

    fn next_seq(subscriber: &mut Subscriber) -> u64 {
        next_event(subscriber).seq
    }

    /// Module whose status change the envelope reports.
    fn module_of(envelope: &EventEnvelope) -> &ModuleId {
        match &envelope.event {
            AppEvent::Module(ModuleEvent::StatusChanged { id, .. }) => id,
            other => panic!("expected a status change, got {other:?}"),
        }
    }

    /// Nothing is waiting for the subscriber right now.
    fn is_idle(subscriber: &mut Subscriber) -> bool {
        poll_once(subscriber.recv()).is_pending()
    }

    /// What a helper thread reports to the test.
    #[derive(Debug, PartialEq, Eq)]
    enum Step {
        /// About to wait for the bus lock held by another thread.
        WaitsForLock,
        /// Its call to the bus returned.
        Done,
    }

    /// Starts a thread that publishes a status change of `module` and stops
    /// once the event has its `seq` but is not sent yet, holding the bus lock.
    /// Returns after the publisher has stopped, with its handle, its `seq` and
    /// a sender that lets it continue.
    fn start_paused_publisher(
        bus: &EventBus,
        module: &'static str,
    ) -> (JoinHandle<Result<u64, BusClosed>>, u64, mpsc::Sender<()>) {
        let (paused_tx, paused) = mpsc::channel();
        let (resume, resume_rx) = mpsc::channel::<()>();
        let bus = bus.clone();
        let publisher = thread::spawn(move || {
            BEFORE_SEND.set(Some(Box::new(move |seq: u64| {
                paused_tx.send(seq).expect("the test waits for the pause");
                resume_rx
                    .recv()
                    .expect("the test lets the publisher continue");
            })));
            bus.publish(status_changed(module))
        });
        let seq = paused.recv().expect("the publisher stops before sending");
        (publisher, seq, resume)
    }

    /// Runs `call` on a new thread that reports [`Step::WaitsForLock`] if it
    /// has to wait for the bus lock and [`Step::Done`] when `call` returns.
    fn spawn_reporting<R: Send + 'static>(
        steps: &mpsc::Sender<Step>,
        call: impl FnOnce() -> R + Send + 'static,
    ) -> JoinHandle<R> {
        let steps = steps.clone();
        thread::spawn(move || {
            let waits = steps.clone();
            BEFORE_WAIT.set(Some(Box::new(move || {
                waits.send(Step::WaitsForLock).expect("the test listens");
            })));
            let result = call();
            steps.send(Step::Done).expect("the test listens");
            result
        })
    }

    #[test]
    fn publish_without_subscribers_is_not_an_error_and_seq_grows() {
        let bus = EventBus::new(4);
        // More events than the capacity: with nobody subscribed nothing lags.
        for expected in 1..=10 {
            assert_eq!(bus.publish(status_changed("t.a")), Ok(expected));
        }
        let mut subscriber = bus.subscribe();
        assert!(
            is_idle(&mut subscriber),
            "events published without subscribers are not delivered later"
        );
        assert_eq!(bus.publish(status_changed("t.a")), Ok(11));
        assert_eq!(next_seq(&mut subscriber), 11);
        assert!(is_idle(&mut subscriber));
    }

    #[test]
    fn envelope_carries_seq_core_source_time_and_event() {
        let bus = EventBus::new(4);
        let mut subscriber = bus.subscribe();
        let failed = ModuleStatus {
            enabled: true,
            state: ModuleState::Failed {
                phase: FailurePhase::Run,
                error: "the module stopped responding".into(),
                at: 42,
            },
        };
        let event = AppEvent::Module(ModuleEvent::StatusChanged {
            id: id("t.a"),
            previous: ModuleStatus {
                enabled: true,
                state: ModuleState::Running,
            },
            current: failed.clone(),
        });

        let before = unix_time_ms();
        let seq = bus.publish(event.clone()).expect("open bus");
        let after = unix_time_ms();

        let envelope = next_event(&mut subscriber);
        assert_eq!(envelope.seq, seq);
        assert_eq!(envelope.source, EventSource::Core);
        assert!((before..=after).contains(&envelope.at_unix_ms));
        assert_eq!(envelope.event, event);
        let AppEvent::Module(ModuleEvent::StatusChanged {
            id: module,
            previous,
            current,
        }) = envelope.event
        else {
            panic!("expected a status change");
        };
        assert_eq!(module, id("t.a"));
        assert_eq!(previous.state, ModuleState::Running);
        assert_eq!(current, failed);
    }

    #[test]
    fn subscribers_receive_the_same_events_in_seq_order() {
        let bus = EventBus::new(8);
        let mut first = bus.subscribe();
        let mut second = bus.subscribe();
        for module in ["t.c", "t.a", "t.b"] {
            bus.publish(status_changed(module)).expect("open bus");
        }
        for subscriber in [&mut first, &mut second] {
            let received: Vec<(u64, ModuleId)> = (0..3)
                .map(|_| {
                    let envelope = next_event(subscriber);
                    (envelope.seq, module_of(&envelope).clone())
                })
                .collect();
            assert_eq!(
                received,
                [(1, id("t.c")), (2, id("t.a")), (3, id("t.b"))],
                "publication order, not module order"
            );
            assert!(is_idle(subscriber));
        }
    }

    #[test]
    fn late_subscriber_receives_only_later_events() {
        let bus = EventBus::new(8);
        let mut early = bus.subscribe();
        bus.publish(status_changed("t.a")).expect("open bus");
        bus.publish(status_changed("t.b")).expect("open bus");

        let mut late = bus.subscribe();
        assert!(is_idle(&mut late), "earlier events are not replayed");
        bus.publish(status_changed("t.c")).expect("open bus");

        assert_eq!(next_seq(&mut late), 3);
        assert!(is_idle(&mut late));
        for seq in 1..=3 {
            assert_eq!(next_seq(&mut early), seq);
        }
        assert!(is_idle(&mut early));
    }

    #[test]
    fn slow_subscriber_lags_by_exact_count_then_continues() {
        let bus = EventBus::new(4);
        let mut slow = bus.subscribe();
        let mut fast = bus.subscribe();
        for seq in 1..=10 {
            assert_eq!(bus.publish(status_changed("t.a")), Ok(seq));
            assert_eq!(next_seq(&mut fast), seq, "a subscriber that keeps up");
        }

        // 10 published, 4 kept: the 6 oldest were dropped for `slow`.
        assert_eq!(ready(&mut slow), Received::Lagged { missed: 6 });
        for seq in 7..=10 {
            assert_eq!(next_seq(&mut slow), seq);
        }
        assert!(is_idle(&mut slow));

        assert_eq!(bus.publish(status_changed("t.a")), Ok(11));
        assert_eq!(next_seq(&mut slow), 11);
        assert_eq!(next_seq(&mut fast), 11);
        assert!(is_idle(&mut fast));
    }

    #[test]
    fn lag_starts_exactly_after_capacity() {
        let bus = EventBus::new(4);
        let mut subscriber = bus.subscribe();
        for _ in 0..4 {
            bus.publish(status_changed("t.a")).expect("open bus");
        }
        for seq in 1..=4 {
            assert_eq!(
                next_seq(&mut subscriber),
                seq,
                "a full buffer loses nothing"
            );
        }

        for _ in 0..5 {
            bus.publish(status_changed("t.a")).expect("open bus");
        }
        assert_eq!(ready(&mut subscriber), Received::Lagged { missed: 1 });
        for seq in 6..=9 {
            assert_eq!(next_seq(&mut subscriber), seq);
        }
    }

    #[test]
    fn dropping_every_handle_closes_the_bus_after_delivery() {
        let bus = EventBus::new(4);
        let clone = bus.clone();
        let mut subscriber = bus.subscribe();

        assert_eq!(bus.publish(status_changed("t.a")), Ok(1));
        drop(bus);
        assert_eq!(
            clone.publish(status_changed("t.a")),
            Ok(2),
            "a remaining handle keeps the bus open"
        );
        drop(clone);

        assert_eq!(next_seq(&mut subscriber), 1);
        assert_eq!(next_seq(&mut subscriber), 2);
        assert_eq!(ready(&mut subscriber), Received::Closed);
        assert_eq!(ready(&mut subscriber), Received::Closed);
    }

    #[test]
    fn close_delivers_published_events_then_closed() {
        let bus = EventBus::new(4);
        let other = bus.clone();
        let mut subscriber = bus.subscribe();
        bus.publish(status_changed("t.a")).expect("open bus");
        bus.publish(status_changed("t.b")).expect("open bus");

        bus.close();
        assert_eq!(
            other.publish(status_changed("t.c")),
            Err(BusClosed),
            "closing affects every clone"
        );
        assert_eq!(next_seq(&mut subscriber), 1);
        assert_eq!(next_seq(&mut subscriber), 2);
        assert_eq!(ready(&mut subscriber), Received::Closed);

        let mut late = other.subscribe();
        assert_eq!(ready(&mut late), Received::Closed);

        bus.close();
        other.close();
        assert_eq!(bus.publish(status_changed("t.a")), Err(BusClosed));
        assert_eq!(ready(&mut subscriber), Received::Closed);
    }

    // Deterministic interleavings: a publisher is stopped at the critical
    // point (its event has a `seq` but is not sent yet) and the test checks
    // what other threads can do meanwhile. No race has to happen by chance.

    #[test]
    fn overlapping_publications_are_delivered_in_seq_order() {
        let bus = EventBus::new(8);
        let mut subscribers = [bus.subscribe(), bus.subscribe(), bus.subscribe()];

        // A has taken seq 1 and stopped right before sending its event.
        let (publisher_a, seq_a, resume_a) = start_paused_publisher(&bus, "t.a");
        assert_eq!(seq_a, 1);

        // B and C publish while A's publication is in progress.
        let (steps_tx, steps) = mpsc::channel();
        let publisher_b = spawn_reporting(&steps_tx, {
            let bus = bus.clone();
            move || bus.publish(status_changed("t.b"))
        });
        let publisher_c = spawn_reporting(&steps_tx, {
            let bus = bus.clone();
            move || bus.publish(status_changed("t.c"))
        });
        for _ in 0..2 {
            assert_eq!(
                steps.recv().expect("B and C make progress"),
                Step::WaitsForLock,
                "no publication can overtake one that already has its seq"
            );
        }

        resume_a.send(()).expect("A continues");
        assert_eq!(publisher_a.join().expect("publisher A"), Ok(1));
        let seq_b = publisher_b.join().expect("publisher B").expect("open bus");
        let seq_c = publisher_c.join().expect("publisher C").expect("open bus");
        let mut later = [seq_b, seq_c];
        later.sort_unstable();
        assert_eq!(
            later,
            [2, 3],
            "the waiting publishers take the next numbers"
        );

        // Every subscriber receives every event in the order of its seq.
        let mut expected = vec![(1, id("t.a")), (seq_b, id("t.b")), (seq_c, id("t.c"))];
        expected.sort();
        for subscriber in &mut subscribers {
            let received: Vec<(u64, ModuleId)> = (0..3)
                .map(|_| {
                    let envelope = next_event(subscriber);
                    (envelope.seq, module_of(&envelope).clone())
                })
                .collect();
            assert_eq!(received, expected);
            assert!(is_idle(subscriber));
        }
    }

    #[test]
    fn close_waits_for_a_publication_in_progress() {
        let bus = EventBus::new(8);
        let mut subscriber = bus.subscribe();
        let (publisher, seq, resume) = start_paused_publisher(&bus, "t.a");
        assert_eq!(seq, 1);

        let (steps_tx, steps) = mpsc::channel();
        let closer = spawn_reporting(&steps_tx, {
            let bus = bus.clone();
            move || bus.close()
        });
        assert_eq!(
            steps.recv().expect("the closing thread makes progress"),
            Step::WaitsForLock,
            "close waits until the publication in progress is sent"
        );

        resume.send(()).expect("the publisher continues");
        assert_eq!(publisher.join().expect("publisher"), Ok(1));
        closer.join().expect("closing thread");
        assert_eq!(steps.recv().expect("close returns"), Step::Done);

        assert_eq!(
            next_seq(&mut subscriber),
            1,
            "the event is delivered before the end of the bus"
        );
        assert_eq!(ready(&mut subscriber), Received::Closed);
        assert_eq!(bus.publish(status_changed("t.b")), Err(BusClosed));
    }

    #[test]
    fn subscription_during_a_publication_starts_after_it() {
        let bus = EventBus::new(8);
        let mut early = bus.subscribe();
        let (publisher, seq, resume) = start_paused_publisher(&bus, "t.a");
        assert_eq!(seq, 1);

        let (steps_tx, steps) = mpsc::channel();
        let subscribing = spawn_reporting(&steps_tx, {
            let bus = bus.clone();
            move || bus.subscribe()
        });
        assert_eq!(
            steps.recv().expect("the subscribing thread makes progress"),
            Step::WaitsForLock,
            "subscribe waits until the publication in progress is sent"
        );

        resume.send(()).expect("the publisher continues");
        assert_eq!(publisher.join().expect("publisher"), Ok(1));
        let mut late = subscribing.join().expect("subscribing thread");
        assert!(
            is_idle(&mut late),
            "the event sent before the subscription is not delivered"
        );

        assert_eq!(bus.publish(status_changed("t.b")), Ok(2));
        assert_eq!(next_seq(&mut late), 2);
        assert_eq!(next_seq(&mut early), 1);
        assert_eq!(next_seq(&mut early), 2);
    }

    // Stress checks: many random interleavings on top of the deterministic
    // scenarios above.

    #[test]
    fn stress_many_publishers_and_readers_agree_on_seq_order() {
        const PUBLISHERS: usize = 8;
        const PER_PUBLISHER: usize = 400;
        const TOTAL: usize = PUBLISHERS * PER_PUBLISHER;
        // Large enough that the readers never lag.
        let bus = EventBus::new(MAX_CAPACITY);

        // Subscribed before any publication; they wait for events on their
        // own threads while the publishers run. A reader stops after `TOTAL`
        // events or, if some are missing, when the bus is closed below.
        let readers: Vec<_> = (0..2)
            .map(|_| {
                let mut subscriber = bus.subscribe();
                thread::spawn(move || {
                    block_on(async move {
                        let mut received = Vec::with_capacity(TOTAL);
                        while received.len() < TOTAL {
                            match subscriber.recv().await {
                                Received::Event(envelope) => received.push(envelope),
                                Received::Closed => break,
                                lagged => panic!("unexpected {lagged:?}"),
                            }
                        }
                        received
                    })
                })
            })
            .collect();

        let start = Arc::new(Barrier::new(PUBLISHERS));
        let publishers: Vec<_> = (0..PUBLISHERS)
            .map(|publisher| {
                let bus = bus.clone();
                let start = Arc::clone(&start);
                thread::spawn(move || {
                    start.wait();
                    (0..PER_PUBLISHER)
                        .map(|n| {
                            let module = format!("p{publisher}.e{n}");
                            bus.publish(status_changed(&module)).expect("open bus")
                        })
                        .collect::<Vec<u64>>()
                })
            })
            .collect();
        let returned: Vec<Vec<u64>> = publishers
            .into_iter()
            .map(|handle| handle.join().expect("publisher thread"))
            .collect();

        for seqs in &returned {
            assert!(
                seqs.windows(2).all(|pair| pair[0] < pair[1]),
                "one publisher's events keep its order"
            );
        }
        let mut all: Vec<u64> = returned.iter().flatten().copied().collect();
        all.sort_unstable();
        let expected: Vec<u64> = (1..=TOTAL as u64).collect();
        assert_eq!(all, expected, "seq numbers are unique and without gaps");

        // Ends the readers' wait if any event is missing.
        bus.close();
        let received: Vec<Vec<EventEnvelope>> = readers
            .into_iter()
            .map(|handle| handle.join().expect("reader thread"))
            .collect();
        for envelopes in &received {
            let seqs: Vec<u64> = envelopes.iter().map(|envelope| envelope.seq).collect();
            assert_eq!(seqs, expected, "delivered in seq order");
            for envelope in envelopes {
                // `pP.eN` was published by publisher P as its event N.
                let (publisher, n) = module_of(envelope)
                    .as_str()
                    .split_once('.')
                    .expect("two segments");
                let publisher: usize = publisher[1..].parse().expect("publisher");
                let n: usize = n[1..].parse().expect("event number");
                assert_eq!(
                    returned[publisher][n], envelope.seq,
                    "the event carries the seq its publisher got back"
                );
            }
        }
        assert_eq!(
            received[0], received[1],
            "every subscriber sees the same events in the same order"
        );
    }

    #[test]
    fn stress_close_racing_with_publishers_loses_no_accepted_event() {
        const PUBLISHERS: usize = 4;
        const ATTEMPTS: usize = 200;
        // Large enough that the subscriber never lags.
        let bus = EventBus::new(1024);
        let mut subscriber = bus.subscribe();

        let start = Arc::new(Barrier::new(PUBLISHERS + 1));
        let publishers: Vec<_> = (0..PUBLISHERS)
            .map(|_| {
                let bus = bus.clone();
                let start = Arc::clone(&start);
                thread::spawn(move || {
                    start.wait();
                    (0..ATTEMPTS)
                        .map(|_| bus.publish(status_changed("t.a")))
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        start.wait();
        bus.close();
        let results: Vec<Vec<Result<u64, BusClosed>>> = publishers
            .into_iter()
            .map(|handle| handle.join().expect("publisher thread"))
            .collect();

        for attempts in &results {
            if let Some(first_rejected) = attempts.iter().position(Result::is_err) {
                assert!(
                    attempts[first_rejected..].iter().all(Result::is_err),
                    "once closed, the bus stays closed"
                );
            }
        }
        let mut accepted: Vec<u64> = results.iter().flatten().filter_map(|r| r.ok()).collect();
        accepted.sort_unstable();
        let expected: Vec<u64> = (1..=accepted.len() as u64).collect();
        assert_eq!(
            accepted, expected,
            "accepted events have unique seq numbers without gaps"
        );

        for seq in expected {
            assert_eq!(
                next_seq(&mut subscriber),
                seq,
                "every accepted event is delivered"
            );
        }
        assert_eq!(ready(&mut subscriber), Received::Closed);
    }

    #[test]
    fn poisoned_lock_is_recovered() {
        let bus = EventBus::new(4);
        let mut subscriber = bus.subscribe();
        let poisoner = bus.clone();
        let result = thread::spawn(move || {
            let _held = poisoner.shared.lock().expect("lock");
            panic!("poison the bus lock");
        })
        .join();
        assert!(result.is_err(), "helper thread must panic");
        assert!(bus.shared.is_poisoned());

        assert_eq!(bus.publish(status_changed("t.a")), Ok(1));
        let mut later = bus.subscribe();
        assert_eq!(bus.publish(status_changed("t.a")), Ok(2));
        bus.close();

        assert_eq!(next_seq(&mut subscriber), 1);
        assert_eq!(next_seq(&mut subscriber), 2);
        assert_eq!(ready(&mut subscriber), Received::Closed);
        assert_eq!(next_seq(&mut later), 2);
        assert_eq!(ready(&mut later), Received::Closed);
    }

    #[test]
    fn capacity_must_be_a_power_of_two_up_to_the_limit() {
        for capacity in [1, 2, 64, DEFAULT_CAPACITY, MAX_CAPACITY] {
            let bus = EventBus::new(capacity);
            assert_eq!(bus.publish(status_changed("t.a")), Ok(1));
        }
        for capacity in [0, 3, 100, MAX_CAPACITY + 1, MAX_CAPACITY * 2] {
            let result = std::panic::catch_unwind(|| EventBus::new(capacity));
            assert!(result.is_err(), "capacity {capacity} must be rejected");
        }
    }
}
