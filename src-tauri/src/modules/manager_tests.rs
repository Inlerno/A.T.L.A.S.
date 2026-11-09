//! Tests of the lifecycle manager (issue #9) on fake modules.
//!
//! Every test runs on a current-thread runtime with paused time: timeouts
//! elapse instantly and deterministically, whatever the machine speed. Every
//! published status change is checked against the status invariant
//! (decision 23) and against the previous status of the same module, and the
//! snapshot against the published changes.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::io::{self, Write};
use std::pin::pin;
use std::sync::{Arc, Mutex, Once};
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use tokio::time::Instant;

use super::contract::{
    FailurePhase, Health, ModuleError, ModuleFactory, ModuleId, ModuleState, ModuleStatus,
};
use super::graph::ModuleGraph;
use super::manager::{
    self, ApplyOutcome, ApplyReport, HEALTH_CHECK_TIMEOUT, INITIALIZE_TIMEOUT, ManagerError,
    ModuleManagerHandle, SHUTDOWN_TIMEOUT, START_TIMEOUT, STOP_TIMEOUT, TASKS_STOP_TIMEOUT, text,
};
use super::plan::{ChangeAction, Confirmation};
use super::testing::{Background, Behavior, FakeFactory, HealthBehavior, Probe, TaskEnd};
use crate::app::{AppState, ExitStep};
use crate::events::bus::{EventBus, Received, Subscriber};
use crate::events::types::{AppEvent, ModuleEvent};

fn run<F: Future>(test: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .start_paused(true)
        .build()
        .expect("test runtime")
        .block_on(test)
}

fn id(value: &str) -> ModuleId {
    ModuleId::new(value).expect("valid module id")
}

fn poll_once<F: Future>(future: F) -> Poll<F::Output> {
    pin!(future).poll(&mut Context::from_waker(Waker::noop()))
}

/// Lets every task run and up to a minute of virtual time pass, which is
/// longer than any single timeout of the manager.
async fn settle() {
    tokio::time::sleep(Duration::from_secs(60)).await;
}

/// Yields until `condition` holds; fails instead of hanging.
async fn wait_until(mut condition: impl FnMut() -> bool) {
    for _ in 0..1000 {
        if condition() {
            return;
        }
        tokio::task::yield_now().await;
    }
    panic!("condition not reached");
}

fn count(counter: &std::sync::atomic::AtomicUsize) -> usize {
    Probe::count(counter)
}

fn confirmed(affected: &[&str]) -> Confirmation {
    Confirmation {
        confirmed: true,
        acknowledged: affected.iter().map(|module| id(module)).collect(),
    }
}

/// Short form of a state for assertions.
fn kind(state: &ModuleState) -> String {
    match state {
        ModuleState::Stopped => "Stopped".into(),
        ModuleState::Initializing => "Initializing".into(),
        ModuleState::Starting => "Starting".into(),
        ModuleState::Running => "Running".into(),
        ModuleState::Stopping => "Stopping".into(),
        ModuleState::Failed { phase, .. } => format!("Failed({phase:?})"),
        ModuleState::Blocked { by, reason } => format!("Blocked({by}, {reason:?})"),
    }
}

/// A status change as (module, enabled, short state).
type Change = (String, bool, String);

fn change(module: &str, enabled: bool, state: &str) -> Change {
    (module.to_owned(), enabled, state.to_owned())
}

fn enable_cycle(module: &str) -> Vec<Change> {
    vec![
        change(module, true, "Stopped"),
        change(module, true, "Initializing"),
        change(module, true, "Starting"),
        change(module, true, "Running"),
    ]
}

fn start_cycle(module: &str) -> Vec<Change> {
    enable_cycle(module).split_off(1)
}

fn stop_cycle(module: &str, enabled: bool) -> Vec<Change> {
    vec![
        change(module, enabled, "Stopping"),
        change(module, enabled, "Stopped"),
    ]
}

fn failed_error(status: &ModuleStatus) -> &str {
    match &status.state {
        ModuleState::Failed { error, .. } => error,
        other => panic!("expected Failed, got {other:?}"),
    }
}

struct Harness {
    manager: ModuleManagerHandle,
    bus: EventBus,
    events: Subscriber,
    probes: BTreeMap<String, Arc<Probe>>,
    /// Last status of each module seen in the events.
    seen: BTreeMap<ModuleId, ModuleStatus>,
}

/// Starts the manager for fake modules `(id, dependencies)` on the current
/// runtime.
fn harness(modules: &[(&str, &[&str])]) -> Harness {
    let bus = EventBus::new(4096);
    let events = bus.subscribe();
    let mut catalog: Vec<Box<dyn ModuleFactory>> = Vec::new();
    let mut probes = BTreeMap::new();
    for (module, dependencies) in modules {
        let (factory, probe) = FakeFactory::new(module, dependencies);
        catalog.push(Box::new(factory));
        probes.insert((*module).to_owned(), probe);
    }
    let (manager, actor) = manager::create(catalog, bus.clone());
    tokio::spawn(actor);
    let seen = manager.statuses();
    Harness {
        manager,
        bus,
        events,
        probes,
        seen,
    }
}

impl Harness {
    fn probe(&self, module: &str) -> &Arc<Probe> {
        &self.probes[module]
    }

    fn status(&self, module: &str) -> ModuleStatus {
        self.manager.statuses()[&id(module)].clone()
    }

    async fn apply(
        &self,
        action: ChangeAction,
        target: &str,
        confirmation: Confirmation,
    ) -> Result<ApplyOutcome, ManagerError> {
        self.manager.apply(action, id(target), confirmation).await
    }

    /// Applies `action`, confirming exactly the affected modules of the
    /// current plan, and expects it to be applied.
    async fn confirm(&self, action: ChangeAction, target: &str) -> ApplyReport {
        let plan = self.manager.plan(action, &id(target)).expect("plan");
        let confirmation = Confirmation {
            confirmed: true,
            acknowledged: plan.affected,
        };
        match self.manager.apply(action, id(target), confirmation).await {
            Ok(ApplyOutcome::Applied(report)) => report,
            other => panic!("{action} {target} not applied: {other:?}"),
        }
    }

    /// Events published since the last call, after the consistency checks.
    fn events(&mut self) -> Vec<ModuleEvent> {
        let mut events = Vec::new();
        while let Poll::Ready(received) = poll_once(self.events.recv()) {
            let Received::Event(envelope) = received else {
                panic!("unexpected {received:?}");
            };
            let AppEvent::Module(event) = envelope.event;
            if let ModuleEvent::StatusChanged {
                id,
                previous,
                current,
            } = &event
            {
                assert!(
                    current.is_consistent(),
                    "inconsistent status of {id}: {current:?}"
                );
                assert_eq!(
                    self.seen.get(id),
                    Some(previous),
                    "a change of {id} must continue from its previous status"
                );
                self.seen.insert(id.clone(), current.clone());
            }
            events.push(event);
        }
        let snapshot = self.manager.statuses();
        assert!(snapshot.values().all(ModuleStatus::is_consistent));
        assert_eq!(
            snapshot, self.seen,
            "the snapshot matches the published changes"
        );
        events
    }

    /// Status changes since the last call, in short form.
    fn changes(&mut self) -> Vec<Change> {
        self.events()
            .into_iter()
            .filter_map(|event| match event {
                ModuleEvent::StatusChanged { id, current, .. } => {
                    Some((id.to_string(), current.enabled, kind(&current.state)))
                }
                ModuleEvent::HealthChanged { .. } => None,
            })
            .collect()
    }

    /// Health results published since the last call.
    fn health_events(&mut self) -> Vec<(String, Health)> {
        self.events()
            .into_iter()
            .filter_map(|event| match event {
                ModuleEvent::HealthChanged { id, health } => Some((id.to_string(), health)),
                ModuleEvent::StatusChanged { .. } => None,
            })
            .collect()
    }
}

#[test]
fn no_module_starts_when_the_manager_starts() {
    run(async {
        let mut h = harness(&[("t.a", &[]), ("t.b", &["t.a"])]);
        settle().await;
        assert!(h.changes().is_empty());
        for module in ["t.a", "t.b"] {
            assert_eq!(h.status(module), ModuleStatus::default());
            assert_eq!(count(&h.probe(module).created), 0);
        }
    });
}

#[test]
fn enable_goes_through_initializing_starting_running() {
    run(async {
        let mut h = harness(&[("t.a", &[])]);
        let report = h.confirm(ChangeAction::Enable, "t.a").await;
        assert_eq!(report.started, [id("t.a")]);
        assert_eq!(h.changes(), enable_cycle("t.a"));
        let probe = h.probe("t.a");
        assert_eq!(
            (
                count(&probe.created),
                count(&probe.initialized),
                count(&probe.started)
            ),
            (1, 1, 1)
        );
        assert_eq!(count(&probe.background), 1);
    });
}

#[test]
fn a_module_subscribes_to_the_bus_through_its_context() {
    run(async {
        let h = harness(&[("t.a", &[])]);
        h.confirm(ChangeAction::Enable, "t.a").await;
        let mut subscriber = h
            .probe("t.a")
            .take_subscriber()
            .expect("subscribed in initialize");
        let mut seen = Vec::new();
        while let Poll::Ready(Received::Event(envelope)) = poll_once(subscriber.recv()) {
            if let AppEvent::Module(ModuleEvent::StatusChanged { current, .. }) = envelope.event {
                seen.push(kind(&current.state));
            }
        }
        assert_eq!(seen, ["Starting", "Running"]);
        let context = h.probe("t.a").context().expect("context");
        assert_eq!(context.id(), &id("t.a"));
    });
}

#[test]
fn every_start_creates_a_new_instance_and_old_ones_are_dropped() {
    run(async {
        let h = harness(&[("t.a", &[])]);
        let probe = Arc::clone(h.probe("t.a"));
        h.confirm(ChangeAction::Enable, "t.a").await;
        h.confirm(ChangeAction::Restart, "t.a").await;
        assert_eq!((count(&probe.created), count(&probe.dropped)), (2, 1));
        h.confirm(ChangeAction::Disable, "t.a").await;
        assert_eq!((count(&probe.created), count(&probe.dropped)), (2, 2));
        h.confirm(ChangeAction::Enable, "t.a").await;
        assert!(probe.end_background(TaskEnd::Fail));
        settle().await;
        assert!(!h.status("t.a").state.is_active());
        assert_eq!((count(&probe.created), count(&probe.dropped)), (3, 3));
        h.confirm(ChangeAction::Recover, "t.a").await;
        assert_eq!((count(&probe.created), count(&probe.dropped)), (4, 3));
        assert_eq!(h.status("t.a").state, ModuleState::Running);
    });
}

#[test]
fn initialize_and_start_failures_fail_the_module_with_the_phase() {
    let cases = [
        (
            FailurePhase::Initialize,
            Behavior::Fail,
            "fake initialize error",
        ),
        (FailurePhase::Initialize, Behavior::Panic, text::PANICKED),
        (FailurePhase::Initialize, Behavior::Hang, text::TIMED_OUT),
        (FailurePhase::Start, Behavior::Fail, "fake start error"),
        (FailurePhase::Start, Behavior::Panic, text::PANICKED),
        (FailurePhase::Start, Behavior::Hang, text::TIMED_OUT),
    ];
    for (phase, behavior, error) in cases {
        run(async {
            let mut h = harness(&[("t.a", &[]), ("t.b", &[])]);
            h.confirm(ChangeAction::Enable, "t.b").await;
            h.probe("t.a").script(|script| match phase {
                FailurePhase::Initialize => script.initialize = behavior,
                _ => script.start = behavior,
            });
            h.changes();

            let started = Instant::now();
            let report = h.confirm(ChangeAction::Enable, "t.a").await;
            let limit = match phase {
                FailurePhase::Initialize => INITIALIZE_TIMEOUT,
                _ => START_TIMEOUT,
            };
            let expected_wait = if behavior == Behavior::Hang {
                limit
            } else {
                Duration::ZERO
            };
            assert_eq!(started.elapsed(), expected_wait, "{phase:?} {behavior:?}");

            assert_eq!(report.failed.len(), 1);
            assert_eq!(report.failed[0].error, error);
            let status = h.status("t.a");
            assert!(status.enabled, "a failed module stays enabled");
            assert_eq!(kind(&status.state), format!("Failed({phase:?})"));
            assert_eq!(failed_error(&status), error);

            let probe = h.probe("t.a");
            settle().await;
            assert_eq!(
                count(&probe.dropped),
                count(&probe.created),
                "instance released"
            );
            assert_eq!(count(&probe.background), 0, "background tasks ended");
            assert_eq!(
                count(&probe.stopped),
                0,
                "stop is not called for an instance that never started"
            );
            assert_eq!(
                h.status("t.b").state,
                ModuleState::Running,
                "other modules keep running"
            );
            assert_eq!(count(&h.probe("t.b").background), 1);
            h.changes();
        });
    }
}

thread_local! {
    /// Log output of the current test thread while it captures logs.
    static CAPTURED: RefCell<Option<Arc<Mutex<Vec<u8>>>>> = const { RefCell::new(None) };
}

/// Writes to the log buffer of the current thread, if it captures logs.
struct ThreadLog;

impl Write for ThreadLog {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        CAPTURED.with_borrow(|buffer| {
            if let Some(buffer) = buffer {
                buffer.lock().expect("log buffer").extend_from_slice(bytes);
            }
        });
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Returns what the current thread logs while `test` runs.
///
/// One global subscriber serves the whole unit-test binary (no unit test sets
/// another one). A thread-local subscriber (`set_default`) would miss events:
/// `tracing` caches per call site, for the whole process, whether a
/// subscriber is interested, and a test running in parallel on another thread
/// can cache "not interested" for a call site of the manager.
fn capture_logs(test: impl FnOnce()) -> String {
    static INSTALL: Once = Once::new();
    INSTALL.call_once(|| {
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .with_writer(|| ThreadLog)
            .finish();
        tracing::subscriber::set_global_default(subscriber)
            .expect("no other global subscriber in unit tests");
    });
    tracing::callsite::rebuild_interest_cache();
    let buffer = Arc::new(Mutex::new(Vec::new()));
    CAPTURED.set(Some(Arc::clone(&buffer)));
    test();
    CAPTURED.set(None);
    let bytes = buffer.lock().expect("log buffer").clone();
    String::from_utf8(bytes).expect("utf-8 log")
}

#[test]
fn panic_details_go_to_the_log_not_to_the_status() {
    let logs = capture_logs(|| {
        run(async {
            let h = harness(&[("t.a", &[]), ("t.b", &[])]);
            h.probe("t.a")
                .script(|script| script.start = Behavior::Panic);
            h.confirm(ChangeAction::Enable, "t.a").await;
            h.confirm(ChangeAction::Enable, "t.b").await;
            assert!(h.probe("t.b").end_background(TaskEnd::Panic));
            settle().await;
            for module in ["t.a", "t.b"] {
                let status = h.status(module);
                assert_eq!(failed_error(&status), text::PANICKED);
            }
        });
    });
    assert!(
        logs.contains("fake start panic in /secret/path/start.rs"),
        "{logs}"
    );
    assert!(
        logs.contains("fake background panic in /secret/path/work.rs"),
        "{logs}"
    );
    assert!(logs.contains("t.a") && logs.contains("t.b"));
}

#[test]
fn a_failed_background_task_blocks_running_dependents_in_reverse_order() {
    run(async {
        let mut h = harness(&[
            ("t.a", &[]),
            ("t.b", &["t.a"]),
            ("t.c", &["t.b"]),
            ("t.d", &[]),
        ]);
        h.confirm(ChangeAction::Enable, "t.c").await;
        h.confirm(ChangeAction::Enable, "t.d").await;
        h.changes();

        assert!(h.probe("t.a").end_background(TaskEnd::Panic));
        settle().await;
        let blocked = "Blocked(t.a, DependencyFailed)";
        assert_eq!(
            h.changes(),
            [
                change("t.a", true, "Failed(Run)"),
                change("t.c", true, "Stopping"),
                change("t.c", true, blocked),
                change("t.b", true, "Stopping"),
                change("t.b", true, blocked),
            ]
        );
        for module in ["t.a", "t.b", "t.c"] {
            let probe = h.probe(module);
            assert_eq!(count(&probe.stopped), 1, "{module}: stop called once");
            assert_eq!(
                count(&probe.background),
                0,
                "{module}: no background task left"
            );
        }
        assert_eq!(h.status("t.d").state, ModuleState::Running);
    });
}

#[test]
fn nothing_restarts_on_its_own() {
    run(async {
        let mut h = harness(&[("t.a", &[]), ("t.b", &["t.a"]), ("t.c", &["t.b"])]);
        h.confirm(ChangeAction::Enable, "t.c").await;
        assert!(h.probe("t.a").end_background(TaskEnd::Fail));
        settle().await;
        h.changes();
        let created: Vec<usize> = ["t.a", "t.b", "t.c"]
            .iter()
            .map(|module| count(&h.probe(module).created))
            .collect();

        // Far past every timeout: nothing starts again.
        tokio::time::sleep(Duration::from_secs(3600)).await;
        assert!(h.changes().is_empty());

        // `recover` needs a confirmation even though the plan is known, and a
        // confirmation of another impact is rejected.
        let unconfirmed = h
            .apply(ChangeAction::Recover, "t.a", Confirmation::default())
            .await;
        assert!(matches!(
            unconfirmed,
            Ok(ApplyOutcome::ConfirmationRequired(_))
        ));
        let partial = h
            .apply(ChangeAction::Recover, "t.a", confirmed(&["t.b"]))
            .await;
        let Ok(ApplyOutcome::ImpactChanged(plan)) = partial else {
            panic!("expected ImpactChanged, got {partial:?}");
        };
        assert_eq!(plan.affected, [id("t.b"), id("t.c")]);
        tokio::time::sleep(Duration::from_secs(3600)).await;
        assert!(h.changes().is_empty());
        let now: Vec<usize> = ["t.a", "t.b", "t.c"]
            .iter()
            .map(|module| count(&h.probe(module).created))
            .collect();
        assert_eq!(now, created, "no instance was created");
    });
}

#[test]
fn recover_is_confirmed_even_alone_and_restarts_blocked_dependents_in_order() {
    run(async {
        let mut h = harness(&[
            ("t.a", &[]),
            ("t.b", &["t.a"]),
            ("t.c", &["t.b"]),
            ("t.x", &[]),
        ]);

        // A single module.
        h.confirm(ChangeAction::Enable, "t.x").await;
        assert!(h.probe("t.x").end_background(TaskEnd::Fail));
        settle().await;
        assert_eq!(failed_error(&h.status("t.x")), "fake background error");
        h.changes();
        let outcome = h
            .apply(ChangeAction::Recover, "t.x", Confirmation::default())
            .await;
        let Ok(ApplyOutcome::ConfirmationRequired(plan)) = outcome else {
            panic!("expected ConfirmationRequired, got {outcome:?}");
        };
        assert!(plan.affected.is_empty());
        assert!(h.changes().is_empty());
        h.apply(ChangeAction::Recover, "t.x", confirmed(&[]))
            .await
            .expect("recover");
        assert_eq!(h.changes(), start_cycle("t.x"));

        // With blocked dependents.
        h.confirm(ChangeAction::Enable, "t.c").await;
        assert!(h.probe("t.a").end_background(TaskEnd::Panic));
        settle().await;
        h.changes();
        let report = h.confirm(ChangeAction::Recover, "t.a").await;
        assert_eq!(report.started, [id("t.a"), id("t.b"), id("t.c")]);
        let expected: Vec<Change> = ["t.a", "t.b", "t.c"]
            .iter()
            .flat_map(|m| start_cycle(m))
            .collect();
        assert_eq!(h.changes(), expected);
    });
}

#[test]
fn restart_stops_dependents_in_reverse_order_and_starts_them_again() {
    run(async {
        let mut h = harness(&[("t.a", &[]), ("t.b", &["t.a"]), ("t.c", &["t.b"])]);
        h.confirm(ChangeAction::Enable, "t.c").await;
        h.changes();
        let report = h.confirm(ChangeAction::Restart, "t.a").await;
        assert_eq!(report.stopped, [id("t.c"), id("t.b"), id("t.a")]);
        assert_eq!(report.started, [id("t.a"), id("t.b"), id("t.c")]);
        let mut expected = Vec::new();
        for module in ["t.c", "t.b", "t.a"] {
            expected.extend(stop_cycle(module, true));
        }
        for module in ["t.a", "t.b", "t.c"] {
            expected.extend(start_cycle(module));
        }
        assert_eq!(h.changes(), expected);
        assert_eq!(
            count(&h.probe("t.a").created),
            2,
            "the target runs a new instance"
        );
    });
}

#[test]
fn a_module_whose_stop_fails_during_restart_is_not_started_again() {
    run(async {
        let mut h = harness(&[("t.a", &[]), ("t.b", &["t.a"]), ("t.c", &["t.b"])]);
        h.confirm(ChangeAction::Enable, "t.c").await;
        h.probe("t.b").script(|script| script.stop = Behavior::Fail);
        h.changes();
        let report = h.confirm(ChangeAction::Restart, "t.a").await;
        assert_eq!(report.stopped, [id("t.c"), id("t.a")]);
        assert_eq!(report.started, [id("t.a")]);
        assert_eq!(report.blocked, [id("t.c")]);
        let mut expected = stop_cycle("t.c", true);
        expected.extend([
            change("t.b", true, "Stopping"),
            change("t.b", true, "Failed(Stop)"),
        ]);
        expected.extend(stop_cycle("t.a", true));
        expected.extend(start_cycle("t.a"));
        expected.push(change("t.c", true, "Blocked(t.b, DependencyFailed)"));
        assert_eq!(h.changes(), expected);
        assert_eq!(
            count(&h.probe("t.b").created),
            1,
            "no retry of the failed module"
        );
    });
}

#[test]
fn a_stale_confirmation_changes_nothing() {
    run(async {
        let mut h = harness(&[("t.a", &[]), ("t.b", &["t.a"])]);
        h.confirm(ChangeAction::Enable, "t.b").await;
        h.changes();
        let outcome = h.apply(ChangeAction::Disable, "t.a", confirmed(&[])).await;
        let Ok(ApplyOutcome::ImpactChanged(plan)) = outcome else {
            panic!("expected ImpactChanged, got {outcome:?}");
        };
        assert_eq!(plan.affected, [id("t.b")]);
        assert!(h.changes().is_empty());
        assert_eq!(h.status("t.a").state, ModuleState::Running);
        assert_eq!(h.status("t.b").state, ModuleState::Running);
    });
}

#[test]
fn a_failure_in_the_middle_of_a_plan_blocks_the_rest_without_retries() {
    run(async {
        let mut h = harness(&[
            ("t.a", &[]),
            ("t.b", &[]),
            ("t.c", &["t.a", "t.b"]),
            ("t.d", &["t.c"]),
        ]);
        h.probe("t.b")
            .script(|script| script.start = Behavior::Fail);
        let report = h.confirm(ChangeAction::Enable, "t.d").await;
        assert_eq!(report.started, [id("t.a")]);
        assert_eq!(report.failed.len(), 1);
        assert_eq!(report.failed[0].id, id("t.b"));
        assert_eq!(report.blocked, [id("t.c"), id("t.d")]);

        let blocked = "Blocked(t.b, DependencyFailed)";
        let mut expected = enable_cycle("t.a");
        expected.extend([
            change("t.b", true, "Stopped"),
            change("t.b", true, "Initializing"),
            change("t.b", true, "Starting"),
            change("t.b", true, "Failed(Start)"),
            change("t.c", true, "Stopped"),
            change("t.c", true, blocked),
            change("t.d", true, "Stopped"),
            change("t.d", true, blocked),
        ]);
        assert_eq!(h.changes(), expected);

        settle().await;
        assert!(h.changes().is_empty(), "no retries");
        assert_eq!(count(&h.probe("t.b").created), 1);
        assert_eq!(count(&h.probe("t.c").created), 0);
        assert_eq!(h.status("t.a").state, ModuleState::Running);
    });
}

#[test]
fn a_plan_with_a_failed_dependency_is_not_applicable() {
    run(async {
        let mut h = harness(&[("t.a", &[]), ("t.b", &["t.a"])]);
        h.confirm(ChangeAction::Enable, "t.a").await;
        assert!(h.probe("t.a").end_background(TaskEnd::Fail));
        settle().await;
        h.changes();
        let outcome = h.apply(ChangeAction::Enable, "t.b", confirmed(&[])).await;
        assert!(
            matches!(outcome, Ok(ApplyOutcome::NotApplicable(ref plan)) if !plan.blockers.is_empty())
        );
        assert!(h.changes().is_empty());
    });
}

#[test]
fn enable_with_disabled_dependencies_needs_confirmation() {
    run(async {
        let mut h = harness(&[("t.a", &[]), ("t.b", &["t.a"])]);
        let outcome = h
            .apply(ChangeAction::Enable, "t.b", Confirmation::default())
            .await;
        let Ok(ApplyOutcome::ConfirmationRequired(plan)) = outcome else {
            panic!("expected ConfirmationRequired, got {outcome:?}");
        };
        assert_eq!(plan.affected, [id("t.a")]);
        settle().await;
        assert!(h.changes().is_empty(), "nothing is enabled");
        assert_eq!(count(&h.probe("t.a").created), 0);
    });
}

#[test]
fn disable_with_dependents_stops_them_in_reverse_order_and_disables_them() {
    run(async {
        let mut h = harness(&[
            ("t.a", &[]),
            ("t.b", &["t.a"]),
            ("t.c", &["t.b"]),
            ("t.d", &[]),
        ]);
        h.confirm(ChangeAction::Enable, "t.c").await;
        h.confirm(ChangeAction::Enable, "t.d").await;
        h.changes();
        let report = h.confirm(ChangeAction::Disable, "t.a").await;
        assert_eq!(report.stopped, [id("t.c"), id("t.b"), id("t.a")]);
        let mut expected = Vec::new();
        for module in ["t.c", "t.b", "t.a"] {
            expected.extend(stop_cycle(module, true));
            expected.push(change(module, false, "Stopped"));
        }
        assert_eq!(h.changes(), expected);
        for module in ["t.a", "t.b", "t.c"] {
            assert_eq!(count(&h.probe(module).background), 0);
        }
        assert_eq!(h.status("t.d").state, ModuleState::Running);
    });
}

#[test]
fn a_failed_stop_does_not_prevent_stopping_the_others() {
    run(async {
        let mut h = harness(&[
            ("t.a", &[]),
            ("t.b", &["t.a"]),
            ("t.c", &["t.a"]),
            ("t.e", &["t.a"]),
        ]);
        for module in ["t.b", "t.c", "t.e"] {
            h.confirm(ChangeAction::Enable, module).await;
        }
        h.probe("t.b").script(|script| script.stop = Behavior::Fail);
        h.probe("t.c")
            .script(|script| script.stop = Behavior::Panic);
        h.probe("t.e").script(|script| script.stop = Behavior::Hang);
        h.changes();

        let report = h.confirm(ChangeAction::Disable, "t.a").await;
        assert_eq!(report.stopped, [id("t.a")]);
        let failures: Vec<(String, String)> = report
            .failed
            .iter()
            .map(|failure| (failure.id.to_string(), failure.error.clone()))
            .collect();
        assert_eq!(
            failures,
            [
                ("t.e".to_owned(), text::TIMED_OUT.to_owned()),
                ("t.c".to_owned(), text::PANICKED.to_owned()),
                ("t.b".to_owned(), "fake stop error".to_owned()),
            ]
        );
        for module in ["t.a", "t.b", "t.c", "t.e"] {
            let status = h.status(module);
            assert!(!status.enabled, "{module} is disabled");
            assert_eq!(
                count(&h.probe(module).stopped),
                1,
                "{module}: stop called once"
            );
            assert_eq!(count(&h.probe(module).background), 0);
        }
        assert_eq!(h.status("t.a").state, ModuleState::Stopped);
        assert_eq!(kind(&h.status("t.e").state), "Failed(Stop)");
        h.changes();
    });
}

#[test]
fn a_repeated_disable_changes_nothing_and_tasks_are_finished() {
    run(async {
        let mut h = harness(&[("t.a", &[])]);
        h.confirm(ChangeAction::Enable, "t.a").await;
        let context = h.probe("t.a").context().expect("context");
        assert!(context.tasks_alive() > 0);
        h.confirm(ChangeAction::Disable, "t.a").await;
        assert_eq!(
            context.tasks_alive(),
            0,
            "background tasks finished (TaskTracker)"
        );
        assert_eq!(count(&h.probe("t.a").background), 0);
        h.changes();

        let again = h.apply(ChangeAction::Disable, "t.a", confirmed(&[])).await;
        assert!(matches!(
            again,
            Err(ManagerError::Module(
                ModuleError::ActionNotApplicable { .. }
            ))
        ));
        settle().await;
        assert!(h.changes().is_empty());
        assert_eq!(h.status("t.a"), ModuleStatus::default());
    });
}

#[test]
fn background_tasks_that_ignore_cancellation_are_aborted() {
    run(async {
        let h = harness(&[("t.a", &[])]);
        h.probe("t.a")
            .script(|script| script.background = Background::IgnoresCancellation);
        h.confirm(ChangeAction::Enable, "t.a").await;
        let started = Instant::now();
        let report = h.confirm(ChangeAction::Disable, "t.a").await;
        assert_eq!(started.elapsed(), TASKS_STOP_TIMEOUT);
        assert_eq!(report.failed[0].error, text::TASKS_ABORTED);
        let status = h.status("t.a");
        assert!(!status.enabled);
        assert_eq!(kind(&status.state), "Failed(Stop)");
        settle().await;
        assert_eq!(count(&h.probe("t.a").background), 0, "aborted");
    });
}

#[test]
fn health_results_are_published_and_never_change_the_state() {
    run(async {
        let mut h = harness(&[("t.a", &[])]);
        h.confirm(ChangeAction::Enable, "t.a").await;
        h.changes();

        assert_eq!(h.manager.check_health(id("t.a")).await, Ok(Health::Healthy));
        assert_eq!(h.manager.check_health(id("t.a")).await, Ok(Health::Healthy));
        let degraded = Health::Degraded("slow disk".into());
        h.probe("t.a")
            .script(|script| script.health = HealthBehavior::Answer(degraded.clone()));
        assert_eq!(
            h.manager.check_health(id("t.a")).await,
            Ok(degraded.clone())
        );
        assert_eq!(
            h.health_events(),
            [
                ("t.a".to_owned(), Health::Healthy),
                ("t.a".to_owned(), degraded)
            ],
            "published when the result changes"
        );
        assert_eq!(h.status("t.a").state, ModuleState::Running);

        let unknown = h.manager.check_health(id("t.z")).await;
        assert!(matches!(
            unknown,
            Err(ManagerError::Module(ModuleError::UnknownModule(_)))
        ));
        h.confirm(ChangeAction::Disable, "t.a").await;
        assert_eq!(
            h.manager.check_health(id("t.a")).await,
            Err(ManagerError::NotRunning(id("t.a")))
        );
    });
}

#[test]
fn a_health_check_without_answer_reports_unhealthy_and_keeps_the_instance() {
    run(async {
        let mut h = harness(&[("t.a", &[])]);
        h.confirm(ChangeAction::Enable, "t.a").await;
        h.changes();
        let late = Duration::from_secs(20);
        h.probe("t.a").script(|script| {
            script.health = HealthBehavior::Late {
                after: late,
                panic: false,
            }
        });

        let started = Instant::now();
        let no_answer = Health::Unhealthy(text::HEALTH_NO_ANSWER.into());
        assert_eq!(
            h.manager.check_health(id("t.a")).await,
            Ok(no_answer.clone())
        );
        assert_eq!(started.elapsed(), HEALTH_CHECK_TIMEOUT);
        // While the late check holds the instance, no second check runs.
        assert_eq!(
            h.manager.check_health(id("t.a")).await,
            Ok(no_answer.clone())
        );
        assert_eq!(count(&h.probe("t.a").health_checks), 1);
        assert_eq!(
            h.status("t.a").state,
            ModuleState::Running,
            "the state does not change"
        );

        // The late answer comes back with the instance.
        settle().await;
        assert_eq!(
            h.health_events(),
            [
                ("t.a".to_owned(), no_answer),
                ("t.a".to_owned(), Health::Healthy)
            ]
        );
        h.probe("t.a")
            .script(|script| script.health = HealthBehavior::Answer(Health::Healthy));
        assert_eq!(h.manager.check_health(id("t.a")).await, Ok(Health::Healthy));
        assert_eq!(count(&h.probe("t.a").health_checks), 2);
        h.confirm(ChangeAction::Disable, "t.a").await;
        assert_eq!(count(&h.probe("t.a").stopped), 1, "stopped normally");
    });
}

#[test]
fn a_module_whose_health_check_never_answers_is_stopped_without_stop() {
    run(async {
        let h = harness(&[("t.a", &[])]);
        h.confirm(ChangeAction::Enable, "t.a").await;
        h.probe("t.a")
            .script(|script| script.health = HealthBehavior::Hang);
        let no_answer = Health::Unhealthy(text::HEALTH_NO_ANSWER.into());
        assert_eq!(h.manager.check_health(id("t.a")).await, Ok(no_answer));

        let started = Instant::now();
        let report = h.confirm(ChangeAction::Disable, "t.a").await;
        assert_eq!(
            started.elapsed(),
            STOP_TIMEOUT,
            "waited for the check within the stop limit"
        );
        assert_eq!(report.failed[0].error, text::UNRESPONSIVE);
        let status = h.status("t.a");
        assert!(!status.enabled);
        assert_eq!(kind(&status.state), "Failed(Stop)");
        assert_eq!(
            count(&h.probe("t.a").stopped),
            0,
            "`stop` needs the instance"
        );
        settle().await;
        let probe = h.probe("t.a");
        assert_eq!(
            count(&probe.dropped),
            count(&probe.created),
            "instance released"
        );
        assert_eq!(count(&probe.background), 0);
    });
}

#[test]
fn a_health_check_panic_fails_the_module_and_blocks_dependents() {
    run(async {
        let mut h = harness(&[("t.a", &[]), ("t.b", &["t.a"])]);
        h.confirm(ChangeAction::Enable, "t.b").await;
        h.changes();
        h.probe("t.a")
            .script(|script| script.health = HealthBehavior::Panic);
        let panicked = Health::Unhealthy(text::HEALTH_PANICKED.into());
        assert_eq!(h.manager.check_health(id("t.a")).await, Ok(panicked));
        assert_eq!(
            h.changes(),
            [
                change("t.a", true, "Failed(HealthCheck)"),
                change("t.b", true, "Stopping"),
                change("t.b", true, "Blocked(t.a, DependencyFailed)"),
            ]
        );
        assert_eq!(
            count(&h.probe("t.a").stopped),
            0,
            "the instance was lost in the panic"
        );
        assert_eq!(count(&h.probe("t.a").background), 0);
    });
}

#[test]
fn a_late_health_check_panic_fails_the_module_at_once() {
    run(async {
        let mut h = harness(&[("t.a", &[]), ("t.b", &["t.a"])]);
        h.confirm(ChangeAction::Enable, "t.b").await;
        let after = Duration::from_secs(20);
        h.probe("t.a")
            .script(|script| script.health = HealthBehavior::Late { after, panic: true });
        let no_answer = Health::Unhealthy(text::HEALTH_NO_ANSWER.into());
        assert_eq!(h.manager.check_health(id("t.a")).await, Ok(no_answer));
        h.changes();
        settle().await;
        assert_eq!(
            h.changes(),
            [
                change("t.a", true, "Failed(HealthCheck)"),
                change("t.b", true, "Stopping"),
                change("t.b", true, "Blocked(t.a, DependencyFailed)"),
            ]
        );
    });
}

/// Splits status changes into complete enable and disable cycles. Panics if
/// the changes of one command are interleaved with those of another.
fn cycles(changes: &[Change]) -> Vec<(String, &'static str)> {
    let mut result = Vec::new();
    let mut rest = changes;
    while let Some(first) = rest.first() {
        let module = first.0.as_str();
        let (name, cycle) = if first == &change(module, true, "Stopped") {
            ("enable", enable_cycle(module))
        } else {
            let mut cycle = stop_cycle(module, true);
            cycle.push(change(module, false, "Stopped"));
            ("disable", cycle)
        };
        assert!(
            rest.len() >= cycle.len() && rest[..cycle.len()] == cycle[..],
            "interleaved changes: {rest:?}"
        );
        result.push((module.to_owned(), name));
        rest = &rest[cycle.len()..];
    }
    result
}

#[test]
fn concurrent_commands_are_handled_one_after_another() {
    run(async {
        let mut h = harness(&[("t.a", &[]), ("t.b", &[])]);
        let commands = [
            (ChangeAction::Enable, "t.a"),
            (ChangeAction::Enable, "t.b"),
            (ChangeAction::Disable, "t.a"),
            (ChangeAction::Enable, "t.a"),
            (ChangeAction::Disable, "t.b"),
            (ChangeAction::Disable, "t.b"),
            (ChangeAction::Enable, "t.a"),
        ];
        let tasks: Vec<_> = commands
            .iter()
            .map(|&(action, target)| {
                let manager = h.manager.clone();
                tokio::spawn(async move { manager.apply(action, id(target), confirmed(&[])).await })
            })
            .collect();
        let health = {
            let manager = h.manager.clone();
            tokio::spawn(async move { manager.check_health(id("t.b")).await })
        };
        let mut applied = 0;
        for task in tasks {
            match task.await.expect("command task") {
                Ok(ApplyOutcome::Applied(_)) => applied += 1,
                Err(ManagerError::Module(ModuleError::ActionNotApplicable { .. })) => {}
                other => panic!("unexpected result {other:?}"),
            }
        }
        let _ = health.await.expect("health task");

        let changes = h.changes();
        let cycles = cycles(&changes);
        assert_eq!(
            cycles.len(),
            applied,
            "one complete cycle per applied command"
        );
        for module in ["t.a", "t.b"] {
            let last = cycles
                .iter()
                .rev()
                .find(|(m, _)| m == module)
                .map(|(_, c)| *c);
            let status = h.status(module);
            match last {
                Some("enable") => assert_eq!(status.state, ModuleState::Running),
                _ => assert_eq!(status, ModuleStatus::default()),
            }
        }
    });
}

#[test]
fn a_report_of_a_stopped_instance_is_ignored() {
    run(async {
        let mut h = harness(&[("t.a", &[])]);
        h.probe("t.a")
            .script(|script| script.background = Background::FailsOnCancel);
        h.confirm(ChangeAction::Enable, "t.a").await;
        h.probe("t.a")
            .script(|script| script.background = Background::Cooperative);
        h.changes();
        // The old instance's task fails while it stops; its report arrives
        // after the new instance has started and must not fail it.
        h.confirm(ChangeAction::Restart, "t.a").await;
        settle().await;
        let mut expected = stop_cycle("t.a", true);
        expected.extend(start_cycle("t.a"));
        assert_eq!(h.changes(), expected);
        assert_eq!(h.status("t.a").state, ModuleState::Running);
    });
}

#[test]
fn a_failure_report_waits_for_the_command_in_progress() {
    run(async {
        let mut h = harness(&[("t.a", &[]), ("t.b", &[])]);
        h.confirm(ChangeAction::Enable, "t.a").await;
        h.probe("t.b")
            .script(|script| script.initialize = Behavior::Hang);
        h.changes();
        let enabling = {
            let manager = h.manager.clone();
            tokio::spawn(async move {
                manager
                    .apply(ChangeAction::Enable, id("t.b"), confirmed(&[]))
                    .await
            })
        };
        wait_until(|| h.status("t.b").state == ModuleState::Initializing).await;
        assert!(h.probe("t.a").end_background(TaskEnd::Panic));
        settle().await;
        enabling.await.expect("enable task").expect("enable");
        assert_eq!(
            h.changes(),
            [
                change("t.b", true, "Stopped"),
                change("t.b", true, "Initializing"),
                change("t.b", true, "Failed(Initialize)"),
                change("t.a", true, "Failed(Run)"),
            ]
        );
    });
}

#[test]
fn shutdown_stops_modules_in_reverse_order_within_the_overall_timeout() {
    run(async {
        let mut h = harness(&[("t.a", &[]), ("t.b", &["t.a"]), ("t.c", &[])]);
        h.confirm(ChangeAction::Enable, "t.b").await;
        h.confirm(ChangeAction::Enable, "t.c").await;
        h.probe("t.b").script(|script| script.stop = Behavior::Hang);
        h.changes();

        let started = Instant::now();
        let report = h.manager.shutdown().await.expect("shutdown");
        assert!(started.elapsed() <= SHUTDOWN_TIMEOUT);
        assert_eq!(started.elapsed(), STOP_TIMEOUT, "only the hung stop waited");
        assert_eq!(report.stopped, [id("t.c"), id("t.a")]);
        assert_eq!(report.failed.len(), 1);
        assert_eq!(report.failed[0].id, id("t.b"));
        assert_eq!(report.failed[0].error, text::TIMED_OUT);

        let mut expected = stop_cycle("t.c", true);
        expected.push(change("t.b", true, "Stopping"));
        expected.push(change("t.b", true, "Failed(Stop)"));
        expected.extend(stop_cycle("t.a", true));
        assert_eq!(
            h.changes(),
            expected,
            "modules stay enabled for the next launch"
        );
    });
}

#[test]
fn at_the_exit_deadline_the_modules_left_are_terminated() {
    run(async {
        let modules: [(&str, &[&str]); 4] =
            [("t.w", &[]), ("t.x", &[]), ("t.y", &[]), ("t.z", &[])];
        let mut h = harness(&modules);
        for (module, _) in modules {
            h.confirm(ChangeAction::Enable, module).await;
        }
        // Exit order z, y, x, w. z hangs until its stop timeout (5 s), y stops
        // after 4 s, x hangs and is cut off at the deadline (10 s), and w gets
        // no turn at all.
        h.probe("t.z").script(|script| script.stop = Behavior::Hang);
        h.probe("t.y")
            .script(|script| script.stop = Behavior::Delay(Duration::from_secs(4)));
        h.probe("t.x").script(|script| script.stop = Behavior::Hang);
        h.probe("t.w").script(|script| script.stop = Behavior::Hang);
        h.changes();

        let started = Instant::now();
        let report = h.manager.shutdown().await.expect("shutdown");
        assert_eq!(started.elapsed(), SHUTDOWN_TIMEOUT);
        assert_eq!(report.stopped, [id("t.y")]);
        let errors: Vec<(String, String)> = report
            .failed
            .iter()
            .map(|failure| (failure.id.to_string(), failure.error.clone()))
            .collect();
        assert_eq!(
            errors,
            [
                ("t.z".to_owned(), text::TIMED_OUT.to_owned()),
                ("t.x".to_owned(), text::SHUTDOWN_DEADLINE.to_owned()),
                ("t.w".to_owned(), text::SHUTDOWN_DEADLINE.to_owned()),
            ]
        );
        assert_eq!(count(&h.probe("t.x").stopped), 1, "x's stop was cut off");
        assert_eq!(
            count(&h.probe("t.w").stopped),
            0,
            "w was terminated without stop"
        );

        let mut expected = vec![
            change("t.z", true, "Stopping"),
            change("t.z", true, "Failed(Stop)"),
        ];
        expected.extend(stop_cycle("t.y", true));
        expected.push(change("t.x", true, "Stopping"));
        expected.push(change("t.x", true, "Failed(Stop)"));
        expected.push(change("t.w", true, "Failed(Stop)"));
        assert_eq!(h.changes(), expected);
        settle().await;
        for (module, _) in modules {
            let probe = h.probe(module);
            assert_eq!(count(&probe.background), 0, "{module}: tasks ended");
            assert_eq!(
                count(&probe.dropped),
                count(&probe.created),
                "{module}: instance released"
            );
        }
    });
}

#[test]
fn exit_interrupts_an_initialize_in_progress() {
    run(async {
        let h = harness(&[("t.a", &[])]);
        h.probe("t.a")
            .script(|script| script.initialize = Behavior::Hang);
        let enabling = {
            let manager = h.manager.clone();
            tokio::spawn(async move {
                manager
                    .apply(ChangeAction::Enable, id("t.a"), confirmed(&[]))
                    .await
            })
        };
        wait_until(|| h.status("t.a").state == ModuleState::Initializing).await;
        let started = Instant::now();
        let report = h.manager.shutdown().await.expect("shutdown");
        assert_eq!(
            started.elapsed(),
            Duration::ZERO,
            "no waiting for the initialize timeout"
        );
        assert!(report.stopped.is_empty() && report.failed.is_empty());
        let Ok(Ok(ApplyOutcome::Applied(applied))) = enabling.await else {
            panic!("enable did not complete");
        };
        assert_eq!(applied.failed[0].error, text::INTERRUPTED);
        assert_eq!(kind(&h.status("t.a").state), "Failed(Initialize)");
    });
}

#[test]
fn exit_skips_the_rest_of_a_plan_in_progress() {
    run(async {
        let mut h = harness(&[("t.a", &[]), ("t.b", &["t.a"]), ("t.c", &["t.b"])]);
        h.probe("t.a")
            .script(|script| script.initialize = Behavior::Hang);
        let enabling = {
            let manager = h.manager.clone();
            tokio::spawn(async move {
                manager
                    .apply(ChangeAction::Enable, id("t.c"), confirmed(&["t.a", "t.b"]))
                    .await
            })
        };
        wait_until(|| h.status("t.a").state == ModuleState::Initializing).await;
        h.manager.shutdown().await.expect("shutdown");
        let Ok(Ok(ApplyOutcome::Applied(report))) = enabling.await else {
            panic!("enable did not complete");
        };
        assert!(report.interrupted);
        assert!(report.started.is_empty() && report.blocked.is_empty());
        assert_eq!(
            h.changes(),
            [
                change("t.a", true, "Stopped"),
                change("t.a", true, "Initializing"),
                change("t.a", true, "Failed(Initialize)"),
            ],
            "nothing after the interrupted step"
        );
        for module in ["t.b", "t.c"] {
            assert_eq!(h.status(module), ModuleStatus::default());
        }
    });
}

#[test]
fn exit_order_stops_modules_before_the_bus_closes_and_nothing_follows() {
    run(async {
        let mut h = harness(&[("t.a", &[]), ("t.b", &[])]);
        h.confirm(ChangeAction::Enable, "t.a").await;
        h.confirm(ChangeAction::Enable, "t.b").await;
        h.probe("t.b").script(|script| script.stop = Behavior::Hang);
        h.changes();
        let state = AppState::new(None, h.bus.clone(), h.manager.clone());

        // A background task fails while the modules are stopping: the report
        // arrives after the stops have begun and is not acted upon.
        let probe = Arc::clone(h.probe("t.a"));
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(1)).await;
            probe.end_background(TaskEnd::Panic);
        });

        assert_eq!(state.request_exit(), ExitStep::StopModules);
        assert_eq!(state.request_exit(), ExitStep::Wait);
        let report = state.modules().shutdown().await.expect("shutdown");
        assert!(state.modules_stopped(), "request the exit again");
        assert_eq!(state.request_exit(), ExitStep::Exit, "a second exit passes");
        assert!(!state.begin_exit(), "Exit has nothing left to stop");
        assert_eq!(report.stopped, [id("t.a")]);
        let expected = [
            change("t.b", true, "Stopping"),
            change("t.b", true, "Failed(Stop)"),
            change("t.a", true, "Stopping"),
            change("t.a", true, "Stopped"),
        ];
        assert_eq!(
            h.changes(),
            expected,
            "every stop is published before the bus closes"
        );

        state.close_events();
        settle().await;
        assert_eq!(poll_once(h.events.recv()), Poll::Ready(Received::Closed));
        let late = h
            .manager
            .apply(ChangeAction::Enable, id("t.a"), confirmed(&[]))
            .await;
        assert_eq!(late, Err(ManagerError::ShutDown));
        assert_eq!(
            h.manager.check_health(id("t.a")).await,
            Err(ManagerError::ShutDown)
        );
        assert_eq!(h.manager.shutdown().await, Err(ManagerError::ShutDown));
        assert_eq!(poll_once(h.events.recv()), Poll::Ready(Received::Closed));
    });
}

#[test]
fn the_status_invariant_holds_through_a_long_scenario() {
    run(async {
        let mut h = harness(&[
            ("t.a", &[]),
            ("t.b", &["t.a"]),
            ("t.c", &["t.b"]),
            ("t.d", &[]),
        ]);
        h.confirm(ChangeAction::Enable, "t.c").await;
        h.events();
        h.confirm(ChangeAction::Enable, "t.d").await;
        h.events();
        assert!(h.probe("t.b").end_background(TaskEnd::Fail));
        settle().await;
        h.events();
        h.confirm(ChangeAction::Disable, "t.c").await;
        h.events();
        h.confirm(ChangeAction::Recover, "t.b").await;
        h.events();
        h.probe("t.a")
            .script(|script| script.health = HealthBehavior::Panic);
        let _ = h.manager.check_health(id("t.a")).await;
        h.events();
        h.confirm(ChangeAction::Disable, "t.b").await;
        h.events();
        h.confirm(ChangeAction::Recover, "t.a").await;
        h.events();
        h.confirm(ChangeAction::Restart, "t.d").await;
        h.events();
        h.probe("t.a")
            .script(|script| script.stop = Behavior::Panic);
        h.manager.shutdown().await.expect("shutdown");
        h.events();
        let statuses = h.manager.statuses();
        assert!(statuses.values().all(ModuleStatus::is_consistent));
        assert_eq!(
            statuses[&id("t.c")],
            ModuleStatus::default(),
            "disabled by the user"
        );
        assert!(matches!(
            statuses[&id("t.a")].state,
            ModuleState::Failed {
                phase: FailurePhase::Stop,
                ..
            }
        ));
        assert!(matches!(
            statuses[&id("t.b")],
            ModuleStatus {
                enabled: false,
                state: ModuleState::Stopped
            }
        ));
    });
}

#[test]
fn an_invalid_catalog_leaves_the_manager_without_modules() {
    run(async {
        let (first, _) = FakeFactory::new("t.a", &[]);
        let (second, _) = FakeFactory::new("t.a", &[]);
        let catalog: Vec<Box<dyn ModuleFactory>> = vec![Box::new(first), Box::new(second)];
        let (manager, actor) = manager::create(catalog, EventBus::new(8));
        tokio::spawn(actor);
        assert!(manager.statuses().is_empty());
        let outcome = manager
            .apply(ChangeAction::Enable, id("t.a"), confirmed(&[]))
            .await;
        assert!(matches!(
            outcome,
            Err(ManagerError::Module(ModuleError::UnknownModule(_)))
        ));
    });
}

#[test]
fn the_built_in_catalog_is_valid() {
    let descriptors = super::catalog()
        .iter()
        .map(|factory| factory.descriptor().clone())
        .collect::<Vec<_>>();
    assert!(ModuleGraph::build(descriptors).is_ok());
}
