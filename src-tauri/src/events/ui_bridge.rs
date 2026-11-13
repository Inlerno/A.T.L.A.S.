//! Bridge from the central event bus to the webview (issue #11).
//!
//! A background task of the core: it subscribes to the bus and forwards
//! module events to the UI as `modules://event`. If it falls behind and the
//! bus drops events for it, it sends `modules://resync` instead, and the UI
//! loads the list again. When the bus closes, the task ends.
//!
//! The bridge only delivers: it never changes a status and never starts an
//! action (decision 25 in #13). The sink is a trait, so tests run it without
//! a webview.

use tauri::{AppHandle, Emitter};

use super::bus::{Received, Subscriber};
use crate::dto::modules::{ResyncDto, UiEventDto};

/// Name of the webview event that carries one module event.
pub const MODULE_EVENT: &str = "modules://event";
/// Name of the webview event that tells the UI to load the modules again.
pub const MODULES_RESYNC: &str = "modules://resync";

/// Where the bridge delivers to. Implementations must not block.
pub trait UiSink: Send + 'static {
    fn event(&self, event: &UiEventDto);
    fn resync(&self, resync: ResyncDto);
}

/// Forwards events from `events` to `sink` until the bus is closed.
pub async fn run(mut events: Subscriber, sink: impl UiSink) {
    loop {
        match events.recv().await {
            Received::Event(envelope) => {
                if let Some(event) = UiEventDto::from_envelope(envelope) {
                    sink.event(&event);
                }
            }
            Received::Lagged { missed } => {
                tracing::warn!(
                    missed,
                    "ui bridge fell behind the event bus: resync requested"
                );
                sink.resync(ResyncDto { missed });
            }
            Received::Closed => break,
        }
    }
    tracing::debug!("ui bridge stopped: the event bus is closed");
}

/// Delivers to every webview of the application.
pub struct WebviewSink(pub AppHandle);

impl UiSink for WebviewSink {
    fn event(&self, event: &UiEventDto) {
        if let Err(error) = self.0.emit(MODULE_EVENT, event) {
            tracing::debug!(%error, seq = event.seq, "module event not delivered to the webview");
        }
    }

    fn resync(&self, resync: ResyncDto) {
        if let Err(error) = self.0.emit(MODULES_RESYNC, resync) {
            tracing::debug!(%error, "resync not delivered to the webview");
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex, PoisonError};

    use super::*;
    use crate::dto::modules::ModuleEventDto;
    use crate::events::bus::EventBus;
    use crate::events::types::{AppEvent, ModuleEvent};
    use crate::modules::contract::{Health, ModuleId, ModuleState, ModuleStatus};

    /// What the UI would have received, in order.
    #[derive(Debug, Clone, PartialEq)]
    enum Sent {
        Event(UiEventDto),
        Resync(u64),
    }

    #[derive(Clone, Default)]
    struct Recorder(Arc<Mutex<Vec<Sent>>>);

    impl Recorder {
        fn sent(&self) -> Vec<Sent> {
            self.0
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone()
        }

        fn push(&self, sent: Sent) {
            self.0
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(sent);
        }
    }

    impl UiSink for Recorder {
        fn event(&self, event: &UiEventDto) {
            self.push(Sent::Event(event.clone()));
        }

        fn resync(&self, resync: ResyncDto) {
            self.push(Sent::Resync(resync.missed));
        }
    }

    fn run_test<F: Future>(test: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("runtime")
            .block_on(test)
    }

    fn id(value: &str) -> ModuleId {
        ModuleId::new(value).expect("valid module id")
    }

    fn publish(bus: &EventBus, event: ModuleEvent) {
        bus.publish(AppEvent::Module(event)).expect("open bus");
    }

    fn settings_changed() -> ModuleEvent {
        ModuleEvent::SettingsChanged {
            id: id("t.a"),
            restart_required: true,
        }
    }

    #[test]
    fn forwards_every_module_event_in_order_and_ends_when_the_bus_closes() {
        run_test(async {
            let bus = EventBus::new(16);
            let recorder = Recorder::default();
            let bridge = tokio::spawn(run(bus.subscribe(), recorder.clone()));
            let running = ModuleStatus {
                enabled: Some(true),
                state: ModuleState::Running,
            };
            publish(
                &bus,
                ModuleEvent::StatusChanged {
                    id: id("t.a"),
                    previous: ModuleStatus::default(),
                    current: running,
                },
            );
            publish(
                &bus,
                ModuleEvent::HealthChanged {
                    id: id("t.a"),
                    health: Health::Degraded("slow".into()),
                },
            );
            publish(&bus, settings_changed());
            publish(&bus, ModuleEvent::ManagerReady);
            bus.close();
            bridge.await.expect("the bridge ends without a panic");

            let sent = recorder.sent();
            let events: Vec<&UiEventDto> = sent
                .iter()
                .map(|sent| match sent {
                    Sent::Event(event) => event,
                    Sent::Resync(_) => panic!("no resync expected: {sent:?}"),
                })
                .collect();
            assert_eq!(
                events.iter().map(|event| event.seq).collect::<Vec<_>>(),
                [1, 2, 3, 4]
            );
            assert!(matches!(
                &events[0].event,
                ModuleEventDto::StatusChanged { id, .. } if id == "t.a"
            ));
            assert!(matches!(
                &events[1].event,
                ModuleEventDto::HealthChanged { .. }
            ));
            assert_eq!(
                events[2].event,
                ModuleEventDto::SettingsChanged {
                    id: "t.a".into(),
                    restart_required: true
                }
            );
            assert_eq!(events[3].event, ModuleEventDto::ManagerReady);
        });
    }

    #[test]
    fn a_lagged_subscription_asks_the_ui_to_resync_and_then_goes_on() {
        run_test(async {
            let bus = EventBus::new(4);
            let recorder = Recorder::default();
            // The bridge does not run yet: ten events overflow its queue of 4.
            let subscriber = bus.subscribe();
            for _ in 0..10 {
                publish(&bus, settings_changed());
            }
            let bridge = tokio::spawn(run(subscriber, recorder.clone()));
            for _ in 0..50 {
                tokio::task::yield_now().await;
            }
            publish(&bus, ModuleEvent::ManagerReady);
            bus.close();
            bridge.await.expect("the bridge ends without a panic");

            let sent = recorder.sent();
            assert_eq!(
                sent[0],
                Sent::Resync(6),
                "the six oldest events were dropped"
            );
            let seqs: Vec<u64> = sent[1..]
                .iter()
                .map(|sent| match sent {
                    Sent::Event(event) => event.seq,
                    Sent::Resync(_) => panic!("one resync only: {sent:?}"),
                })
                .collect();
            assert_eq!(seqs, [7, 8, 9, 10, 11], "delivery goes on after the resync");
        });
    }

    #[test]
    fn a_bus_closed_before_the_bridge_starts_ends_it_at_once() {
        run_test(async {
            let bus = EventBus::new(4);
            bus.close();
            let recorder = Recorder::default();
            run(bus.subscribe(), recorder.clone()).await;
            assert!(recorder.sent().is_empty());
        });
    }
}
