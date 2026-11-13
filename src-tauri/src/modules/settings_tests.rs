//! Tests of module configuration, startup and the exit rules of MM-5A
//! (issue #10, ADR-015) on fake modules and the test store.
//!
//! As in `manager_tests`, every test runs on a current-thread runtime with
//! paused time, and every published status change is checked against the
//! status invariant and the snapshot. Numbers in the comments refer to the
//! test list of the MM-5 plan (section 7).

use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::time::Instant;

use super::config::{MAX_SETTINGS_BYTES, MAX_SETTINGS_DEPTH, ModuleConfig, SettingsProblem};
use super::contract::{ModuleError, ModuleState, ModuleStatus};
use super::manager::{
    ApplyOutcome, INITIALIZE_TIMEOUT, ManagerError, SHUTDOWN_TIMEOUT, STOP_TIMEOUT, StopReport,
    TASKS_STOP_TIMEOUT, text,
};
use super::plan::ChangeAction;
use super::testing::*;
use crate::events::types::ModuleEvent;

const ALL_ACTIONS: [ChangeAction; 4] = [
    ChangeAction::Enable,
    ChangeAction::Disable,
    ChangeAction::Recover,
    ChangeAction::Restart,
];

/// Spawns the manager and waits until it is ready; the events of the
/// preparation are left for the test.
async fn started(
    modules: &[(&str, &[&str])],
    enabled_by_default: &[&str],
    store: &TestStore,
) -> Harness {
    let h = Harness::spawn(modules, enabled_by_default, store.clone());
    h.ready().await;
    h
}

/// Loading change of each module: its decision, `Stopped`.
fn loaded(modules: &[(&str, bool)]) -> Vec<Change> {
    modules
        .iter()
        .map(|(module, enabled)| change(module, *enabled, "Stopped"))
        .collect()
}

fn settings_of(h: &Harness, module: &str) -> Value {
    Value::clone(&h.manager.get_settings(&id(module)).expect("settings").value)
}

fn record(enabled: bool, settings: Value) -> Option<ModuleConfig> {
    Some(ModuleConfig { enabled, settings })
}

fn ready_events(events: &[ModuleEvent]) -> usize {
    events
        .iter()
        .filter(|event| **event == ModuleEvent::ManagerReady)
        .count()
}

fn position(events: &[ModuleEvent], wanted: impl Fn(&ModuleEvent) -> bool) -> usize {
    events
        .iter()
        .position(wanted)
        .unwrap_or_else(|| panic!("event not found in {events:?}"))
}

fn is_change(event: &ModuleEvent, module: &str, state: &str) -> bool {
    matches!(event, ModuleEvent::StatusChanged { id, current, .. }
        if id.as_str() == module && kind(&current.state) == state)
}

fn is_unavailable(status: &ModuleStatus) -> bool {
    status.enabled.is_none() && matches!(status.state, ModuleState::ConfigUnavailable { .. })
}

// ---------------------------------------------------------------------------
// Loading and readiness
// ---------------------------------------------------------------------------

/// 1. No record: `default_enabled` and the factory's defaults; nothing is
///    written by the loading.
#[test]
fn a_module_without_stored_config_gets_the_defaults() {
    run(async {
        let store = TestStore::default();
        let h = Harness::spawn(&[("t.a", &[]), ("t.b", &[])], &["t.b"], store.clone());
        h.probe("t.b")
            .factory(|factory| factory.defaults = json!({"level": 1}));
        h.ready().await;
        assert_eq!(h.status("t.a"), ModuleStatus::default());
        assert_eq!(h.status("t.b").state, ModuleState::Running);
        assert_eq!(settings_of(&h, "t.a"), json!({}));
        assert_eq!(settings_of(&h, "t.b"), json!({"level": 1}));
        assert_eq!(h.probe("t.b").settings_seen(), [json!({"level": 1})]);
        assert!(store.records().is_empty(), "loading never writes");
        assert!(store.batches().is_empty());
    });
}

/// 2. A stored record wins over the defaults.
#[test]
fn enabled_and_settings_are_loaded_from_the_store() {
    run(async {
        let store = TestStore::default();
        store.put("t.a", true, json!({"x": 1}));
        store.put("t.b", false, json!({}));
        let h = started(&[("t.a", &[]), ("t.b", &[])], &["t.b"], &store).await;
        assert_eq!(h.status("t.a").enabled, Some(true));
        assert_eq!(h.status("t.a").state, ModuleState::Running);
        assert_eq!(
            h.status("t.b"),
            ModuleStatus::default(),
            "stored `false` wins"
        );
        assert_eq!(settings_of(&h, "t.a"), json!({"x": 1}));
        assert_eq!(h.probe("t.a").settings_seen(), [json!({"x": 1})]);
        assert_eq!(count(&h.probe("t.b").created), 0);
    });
}

/// 3. One `StatusChanged` per module from `{ None, Stopped }`, in start
///    order, no `SettingsChanged`, then `ManagerReady` once.
#[test]
fn loading_announces_each_decision_once() {
    run(async {
        let store = TestStore::default();
        store.put("t.c", false, json!({"k": 1}));
        let mut h = started(
            &[("t.a", &[]), ("t.b", &["t.a"]), ("t.c", &[])],
            &[],
            &store,
        )
        .await;
        let events = h.events();
        assert_eq!(
            events.len(),
            4,
            "three loading changes and `ManagerReady`: {events:?}"
        );
        for (event, module) in events.iter().zip(["t.a", "t.b", "t.c"]) {
            let ModuleEvent::StatusChanged {
                id: changed,
                previous,
                current,
            } = event
            else {
                panic!("expected a loading change, got {event:?}");
            };
            assert_eq!(changed.as_str(), module);
            assert_eq!(previous.enabled, None);
            assert_eq!(previous.state, ModuleState::Stopped);
            assert_eq!(*current, ModuleStatus::default());
        }
        assert_eq!(events[3], ModuleEvent::ManagerReady);
        assert_eq!(store.loads(), ["t.a", "t.b", "t.c"]);
    });
}

/// 4. Until the manager is ready, readers get `NotReady`.
#[test]
fn reads_before_ready_return_not_ready() {
    run(async {
        let store = TestStore::default();
        store.hang_load("t.b");
        let h = Harness::spawn(&[("t.a", &[]), ("t.b", &[])], &[], store.clone());
        let check = |h: &Harness| {
            assert!(!h.manager.is_ready());
            assert_eq!(h.manager.statuses(), Err(ManagerError::NotReady));
            assert_eq!(
                h.manager.plan(ChangeAction::Enable, &id("t.a")),
                Err(ManagerError::NotReady)
            );
            assert_eq!(
                h.manager.get_settings(&id("t.a")),
                Err(ManagerError::NotReady)
            );
        };
        check(&h);
        settle().await;
        assert_eq!(store.loads(), ["t.a", "t.b"], "t.b hangs");
        check(&h);
    });
}

/// 5. Commands sent while the manager prepares wait and then run in order.
#[test]
fn commands_sent_before_ready_run_after_the_preparation_in_order() {
    run(async {
        let store = TestStore::default();
        store.put("t.a", true, json!({}));
        let mut h = Harness::spawn(&[("t.a", &[]), ("t.b", &[])], &[], store.clone());
        h.probe("t.a")
            .script(|script| script.initialize = Behavior::Delay(Duration::from_secs(1)));
        let manager = h.manager.clone();
        let enable = tokio::spawn(async move {
            manager
                .apply(ChangeAction::Enable, id("t.b"), confirmed(&[]))
                .await
        });
        let manager = h.manager.clone();
        let update =
            tokio::spawn(async move { manager.update_settings(id("t.b"), json!({"v": 2})).await });
        h.ready().await;
        assert!(matches!(
            enable.await.expect("task"),
            Ok(ApplyOutcome::Applied(_))
        ));
        assert_eq!(update.await.expect("task"), Ok(()));
        let events = h.events();
        let ready = position(&events, |event| *event == ModuleEvent::ManagerReady);
        let a_running = position(&events, |event| is_change(event, "t.a", "Running"));
        let b_running = position(&events, |event| is_change(event, "t.b", "Running"));
        let settings = position(
            &events,
            |event| matches!(event, ModuleEvent::SettingsChanged { id, .. } if id.as_str() == "t.b"),
        );
        assert!(a_running < ready && ready < b_running && b_running < settings);
    });
}

/// 6. `ManagerReady` comes once, after the startup, and never again.
#[test]
fn manager_ready_is_published_once_after_autostart() {
    run(async {
        let store = TestStore::default();
        store.put("t.a", true, json!({}));
        let mut h = started(&[("t.a", &[]), ("t.b", &[])], &[], &store).await;
        let events = h.events();
        assert_eq!(ready_events(&events), 1);
        assert_eq!(events.last(), Some(&ModuleEvent::ManagerReady));
        h.confirm(ChangeAction::Enable, "t.b").await;
        h.confirm(ChangeAction::Restart, "t.a").await;
        settle().await;
        assert_eq!(ready_events(&h.events()), 0);
    });
}

/// 7. Records of modules that are not in the catalog are neither read nor
///    changed.
#[test]
fn unknown_records_are_left_untouched() {
    run(async {
        let store = TestStore::default();
        store.put("old.module", true, json!({"k": "v"}));
        let h = started(&[("t.a", &[])], &[], &store).await;
        h.confirm(ChangeAction::Enable, "t.a").await;
        h.manager
            .update_settings(id("t.a"), json!({"n": 1}))
            .await
            .expect("update");
        h.confirm(ChangeAction::Disable, "t.a").await;
        assert_eq!(store.loads(), ["t.a"]);
        assert_eq!(store.record("old.module"), record(true, json!({"k": "v"})));
        assert!(
            store
                .batches()
                .iter()
                .flatten()
                .all(|(module, _)| module == "t.a")
        );
    });
}

/// 8. An exit during the loading: the snapshot is never declared reliable.
#[test]
fn an_exit_during_loading_never_reports_ready() {
    run(async {
        let store = TestStore::default();
        store.hang_load("t.b");
        let mut h = Harness::spawn(
            &[("t.a", &[]), ("t.b", &[]), ("t.c", &[])],
            &[],
            store.clone(),
        );
        let loads = store.clone();
        wait_until_with_pool(move || loads.loads().len() == 2).await;
        let manager = h.manager.clone();
        let enable = tokio::spawn(async move {
            manager
                .apply(ChangeAction::Enable, id("t.c"), confirmed(&[]))
                .await
        });
        tokio::task::yield_now().await;
        let begun = Instant::now();
        let report = h.manager.shutdown().await.expect("first exit request");
        assert_eq!(
            begun.elapsed(),
            Duration::ZERO,
            "the read was not waited for"
        );
        assert_eq!(report, StopReport::default());
        assert_eq!(enable.await.expect("task"), Err(ManagerError::ShutDown));

        assert!(!h.manager.is_ready());
        assert_eq!(h.manager.statuses(), Err(ManagerError::NotReady));
        assert_eq!(
            h.manager.plan(ChangeAction::Enable, &id("t.c")),
            Err(ManagerError::NotReady)
        );
        assert_eq!(
            h.manager.get_settings(&id("t.a")),
            Err(ManagerError::NotReady)
        );
        let events = h.events();
        assert_eq!(ready_events(&events), 0);
        assert_eq!(
            h.seen[&id("t.a")],
            ModuleStatus::default(),
            "only t.a was loaded"
        );
        for module in ["t.b", "t.c"] {
            assert_eq!(h.seen[&id(module)].enabled, None, "{module}");
            assert_eq!(h.seen[&id(module)].state, ModuleState::Stopped, "{module}");
        }
        assert_eq!(events.len(), 1, "{events:?}");
        for module in ["t.a", "t.b", "t.c"] {
            assert_eq!(count(&h.probe(module).created), 0);
        }
        assert!(store.batches().is_empty());
        settle().await;
        assert!(h.events().is_empty(), "nothing after the reply");
    });
}

/// 9. An exit during the startup: the interrupted module fails as in MM-4,
///    then `ManagerReady`, then only `shutdown_all`; every command gets
///    `ShutDown`.
#[test]
fn an_exit_during_autostart_reports_ready_before_stopping() {
    run(async {
        let store = TestStore::default();
        for module in ["t.a", "t.b", "t.c", "t.d"] {
            store.put(module, true, json!({}));
        }
        store.put("t.e", false, json!({}));
        let mut h = Harness::spawn(
            &[
                ("t.a", &[]),
                ("t.b", &["t.a"]),
                ("t.c", &["t.b"]),
                ("t.d", &[]),
                ("t.e", &[]),
            ],
            &[],
            store.clone(),
        );
        h.probe("t.b")
            .script(|script| script.initialize = Behavior::Hang);
        let probe_b = Arc::clone(h.probe("t.b"));
        wait_until_with_pool(move || count(&probe_b.initialized) == 1).await;

        let m = h.manager.clone();
        let enable_e = tokio::spawn(async move {
            m.apply(ChangeAction::Enable, id("t.e"), confirmed(&[]))
                .await
        });
        let m = h.manager.clone();
        let update_d =
            tokio::spawn(async move { m.update_settings(id("t.d"), json!({"k": 2})).await });
        let m = h.manager.clone();
        let health_a = tokio::spawn(async move { m.check_health(id("t.a")).await });
        tokio::task::yield_now().await;
        let begun = Instant::now();
        let m = h.manager.clone();
        let exit = tokio::spawn(async move { m.shutdown().await });
        tokio::task::yield_now().await;
        let m = h.manager.clone();
        let disable_a = tokio::spawn(async move {
            m.apply(ChangeAction::Disable, id("t.a"), confirmed(&[]))
                .await
        });

        let report = exit.await.expect("task").expect("first exit request");
        // 3. Not waited for `INITIALIZE_TIMEOUT`.
        assert!(
            begun.elapsed() < INITIALIZE_TIMEOUT,
            "{:?}",
            begun.elapsed()
        );
        // 10. Commands before and after the request: `ShutDown`, no effect.
        assert_eq!(enable_e.await.expect("task"), Err(ManagerError::ShutDown));
        assert_eq!(update_d.await.expect("task"), Err(ManagerError::ShutDown));
        assert_eq!(health_a.await.expect("task"), Err(ManagerError::ShutDown));
        assert_eq!(disable_a.await.expect("task"), Err(ManagerError::ShutDown));
        assert!(store.batches().is_empty());
        assert_eq!(count(&h.probe("t.a").health_checks), 0);
        assert_eq!(settings_of(&h, "t.d"), json!({}));
        // 11. Only t.a was stopped.
        assert_eq!(
            report,
            StopReport {
                stopped: vec![id("t.a")],
                failed: Vec::new()
            }
        );

        let events = h.events();
        // 1. Every module loaded once.
        assert_eq!(store.loads(), ["t.a", "t.b", "t.c", "t.d", "t.e"]);
        // 7–9. One `ManagerReady`, after t.b failed, before the first stop.
        assert_eq!(ready_events(&events), 1);
        let failed_b = position(&events, |event| {
            is_change(event, "t.b", "Failed(Initialize)")
        });
        let ready = position(&events, |event| *event == ModuleEvent::ManagerReady);
        let stopping_a = position(&events, |event| is_change(event, "t.a", "Stopping"));
        assert!(failed_b < ready && ready < stopping_a, "{events:?}");
        assert_eq!(
            events.len(),
            ready + 3,
            "after ready only t.a stops: {events:?}"
        );
        assert!(
            events
                .iter()
                .all(|event| matches!(event, ModuleEvent::StatusChanged { .. })
                    || *event == ModuleEvent::ManagerReady)
        );
        // 2, 4. t.a ran; t.b interrupted with the MM-4 text, no start or stop.
        let b = h.status("t.b");
        assert_eq!(b.enabled, Some(true));
        assert_eq!(kind(&b.state), "Failed(Initialize)");
        assert_eq!(failed_error(&b), text::INTERRUPTED);
        let probe_b = h.probe("t.b");
        assert_eq!(count(&probe_b.initialized), 1);
        assert_eq!(count(&probe_b.started), 0);
        assert_eq!(count(&probe_b.stopped), 0);
        // 5. Not reached and disabled modules: unchanged since the loading.
        for module in ["t.c", "t.d"] {
            assert_eq!(
                h.status(module),
                ModuleStatus {
                    enabled: Some(true),
                    state: ModuleState::Stopped
                }
            );
            assert_eq!(count(&h.probe(module).created), 0);
        }
        assert_eq!(h.status("t.e"), ModuleStatus::default());
        // 6. Ready, and readable after the reply.
        assert!(h.manager.is_ready());
        assert_eq!(h.status("t.a").state, ModuleState::Stopped);
        // 3, 12. The instance of t.b is dropped; nothing after the reply.
        settle().await;
        assert_eq!(count(&probe_b.dropped), count(&probe_b.created));
        assert!(h.events().is_empty());
        assert_eq!(h.manager.shutdown().await, Err(ManagerError::ShutDown));
        assert_eq!(
            h.manager
                .apply(ChangeAction::Enable, id("t.e"), confirmed(&[]))
                .await,
            Err(ManagerError::ShutDown)
        );
        assert!(h.events().is_empty());
    });
}

/// 10. `ready` stays true after the exit, but commands are refused.
#[test]
fn ready_does_not_promise_that_commands_are_accepted() {
    run(async {
        let h = harness(&[("t.a", &[])]).await;
        h.manager.shutdown().await.expect("exit");
        assert!(h.manager.is_ready());
        assert_eq!(h.status("t.a"), ModuleStatus::default());
        assert_eq!(
            h.manager
                .apply(ChangeAction::Enable, id("t.a"), confirmed(&[]))
                .await,
            Err(ManagerError::ShutDown)
        );
        assert_eq!(
            h.manager.update_settings(id("t.a"), json!({})).await,
            Err(ManagerError::ShutDown)
        );
    });
}

// ---------------------------------------------------------------------------
// Startup of enabled modules
// ---------------------------------------------------------------------------

/// 11. Replaces MM-4's "no module starts when the manager starts": only the
///     modules the user left enabled start; disabled ones are untouched.
#[test]
fn only_enabled_modules_start_with_the_manager() {
    run(async {
        let store = TestStore::default();
        let mut h = started(
            &[("t.a", &[]), ("t.b", &["t.a"]), ("t.c", &[])],
            &["t.a"],
            &store,
        )
        .await;
        let mut expected = loaded(&[("t.a", true), ("t.b", false), ("t.c", false)]);
        expected.extend(start_cycle("t.a"));
        assert_eq!(h.changes(), expected);
        for module in ["t.b", "t.c"] {
            assert_eq!(h.status(module), ModuleStatus::default());
            assert_eq!(count(&h.probe(module).created), 0);
        }
        settle().await;
        assert!(h.changes().is_empty());
    });
}

/// 12. Dependencies start first.
#[test]
fn autostart_follows_the_dependency_order() {
    run(async {
        let store = TestStore::default();
        for module in ["t.a", "t.b", "t.c"] {
            store.put(module, true, json!({}));
        }
        let mut h = started(
            &[("t.c", &["t.b"]), ("t.b", &["t.a"]), ("t.a", &[])],
            &[],
            &store,
        )
        .await;
        let mut expected = loaded(&[("t.a", true), ("t.b", true), ("t.c", true)]);
        for module in ["t.a", "t.b", "t.c"] {
            expected.extend(start_cycle(module));
        }
        assert_eq!(h.changes(), expected);
    });
}

/// 13. A disabled dependency blocks; it is never enabled implicitly.
#[test]
fn an_enabled_module_with_a_disabled_dependency_is_blocked() {
    run(async {
        let store = TestStore::default();
        store.put("t.a", false, json!({}));
        store.put("t.b", true, json!({}));
        let h = started(&[("t.a", &[]), ("t.b", &["t.a"])], &[], &store).await;
        assert_eq!(h.status("t.a"), ModuleStatus::default());
        assert_eq!(
            kind(&h.status("t.b").state),
            "Blocked(t.a, DependencyDisabled)"
        );
        assert_eq!(h.status("t.b").enabled, Some(true));
        for module in ["t.a", "t.b"] {
            assert_eq!(count(&h.probe(module).created), 0);
        }
        assert!(store.batches().is_empty(), "no implicit decision is saved");
    });
}

/// 14–15. A startup failure blocks the enabled dependents, naming the root
///        cause; a disabled root is named the same way.
#[test]
fn a_startup_failure_blocks_enabled_dependents_naming_the_root_cause() {
    run(async {
        let store = TestStore::default();
        for module in ["t.a", "t.b", "t.c"] {
            store.put(module, true, json!({}));
        }
        let h = Harness::spawn(
            &[("t.a", &[]), ("t.b", &["t.a"]), ("t.c", &["t.b"])],
            &[],
            store.clone(),
        );
        h.probe("t.a")
            .script(|script| script.initialize = Behavior::Fail);
        h.ready().await;
        assert_eq!(kind(&h.status("t.a").state), "Failed(Initialize)");
        for module in ["t.b", "t.c"] {
            assert_eq!(
                kind(&h.status(module).state),
                "Blocked(t.a, DependencyFailed)",
                "{module}"
            );
            assert_eq!(count(&h.probe(module).created), 0);
        }
    });
    run(async {
        let store = TestStore::default();
        store.put("t.a", false, json!({}));
        store.put("t.b", true, json!({}));
        store.put("t.c", true, json!({}));
        let h = started(
            &[("t.a", &[]), ("t.b", &["t.a"]), ("t.c", &["t.b"])],
            &[],
            &store,
        )
        .await;
        assert_eq!(
            kind(&h.status("t.c").state),
            "Blocked(t.a, DependencyDisabled)",
            "the root cause, not the nearest dependency"
        );
    });
}

/// 16. A startup failure is not retried, however long the manager runs.
#[test]
fn a_startup_failure_is_not_retried() {
    run(async {
        let store = TestStore::default();
        store.put("t.a", true, json!({}));
        store.put("t.b", true, json!({}));
        let mut h = Harness::spawn(&[("t.a", &[]), ("t.b", &["t.a"])], &[], store.clone());
        h.probe("t.a")
            .script(|script| script.start = Behavior::Panic);
        h.ready().await;
        h.changes();
        for _ in 0..30 {
            settle().await;
        }
        assert!(h.changes().is_empty());
        assert_eq!(count(&h.probe("t.a").created), 1);
        assert_eq!(count(&h.probe("t.b").created), 0);
        assert_eq!(kind(&h.status("t.a").state), "Failed(Start)");
    });
}

/// 17. The next run reads the decisions saved by this one.
#[test]
fn a_new_manager_with_the_same_store_starts_enabled_modules_again() {
    run(async {
        let store = TestStore::default();
        let h = started(&[("t.a", &[]), ("t.b", &["t.a"])], &[], &store).await;
        h.confirm(ChangeAction::Enable, "t.b").await;
        h.manager.shutdown().await.expect("exit");

        let mut next = started(&[("t.a", &[]), ("t.b", &["t.a"])], &[], &store).await;
        let mut expected = loaded(&[("t.a", true), ("t.b", true)]);
        expected.extend(start_cycle("t.a"));
        expected.extend(start_cycle("t.b"));
        assert_eq!(next.changes(), expected);
    });
}

/// 18. `Failed` and `Blocked` belong to one run only.
#[test]
fn failed_and_blocked_do_not_carry_over() {
    run(async {
        let store = TestStore::default();
        store.put("t.a", true, json!({}));
        store.put("t.b", true, json!({}));
        let h = started(&[("t.a", &[]), ("t.b", &["t.a"])], &[], &store).await;
        assert!(h.probe("t.a").end_background(TaskEnd::Fail));
        settle().await;
        assert_eq!(kind(&h.status("t.a").state), "Failed(Run)");
        assert_eq!(
            kind(&h.status("t.b").state),
            "Blocked(t.a, DependencyFailed)"
        );
        h.manager.shutdown().await.expect("exit");

        let next = started(&[("t.a", &[]), ("t.b", &["t.a"])], &[], &store).await;
        for module in ["t.a", "t.b"] {
            assert_eq!(next.status(module).state, ModuleState::Running, "{module}");
        }
    });
}

// ---------------------------------------------------------------------------
// Configuration that cannot be read
// ---------------------------------------------------------------------------

/// 19. A read error: the decision stays unknown, no defaults, no instance,
///     no action applies; the details go to the log only.
#[test]
fn a_read_error_leaves_the_decision_unknown() {
    let logs = capture_logs(|| {
        run(async {
            let store = TestStore::default();
            store.fail_load("t.a");
            let mut h = started(&[("t.a", &[])], &["t.a"], &store).await;
            assert_eq!(
                h.changes(),
                [unknown("t.a", "ConfigUnavailable")],
                "default_enabled is not used"
            );
            let status = h.status("t.a");
            assert!(is_unavailable(&status));
            let ModuleState::ConfigUnavailable { error, .. } = &status.state else {
                unreachable!();
            };
            assert_eq!(error, text::CONFIG_UNREADABLE);
            assert_eq!(count(&h.probe("t.a").created), 0);
            assert_eq!(
                h.manager.get_settings(&id("t.a")),
                Err(ManagerError::ConfigUnavailable(id("t.a")))
            );
            assert_eq!(
                h.manager.update_settings(id("t.a"), json!({})).await,
                Err(ManagerError::ConfigUnavailable(id("t.a")))
            );
            for action in ALL_ACTIONS {
                let refused = ManagerError::Module(ModuleError::ActionNotApplicable {
                    module: id("t.a"),
                    action,
                });
                assert_eq!(h.manager.plan(action, &id("t.a")), Err(refused.clone()));
                assert_eq!(
                    h.manager.apply(action, id("t.a"), confirmed(&[])).await,
                    Err(refused)
                );
            }
            assert_eq!(
                h.manager.check_health(id("t.a")).await,
                Err(ManagerError::NotRunning(id("t.a")))
            );
            settle().await;
            assert!(h.changes().is_empty(), "no recovery within the run");
            assert!(store.batches().is_empty());
        });
    });
    let line = log_line(&logs, "module configuration could not be read");
    assert!(
        line.contains("ERROR") && line.contains("/secret/path/config.db"),
        "{line}"
    );
    assert!(log_line(&logs, "module configuration loaded").contains("unavailable=1"));
}

/// 20. Dependents of an unreadable module are blocked by it and cannot be
///     enabled or recovered.
#[test]
fn dependents_are_blocked_by_an_unavailable_configuration() {
    run(async {
        let store = TestStore::default();
        store.fail_load("t.a");
        store.put("t.b", true, json!({}));
        store.put("t.c", true, json!({}));
        store.put("t.d", false, json!({}));
        let h = started(
            &[
                ("t.a", &[]),
                ("t.b", &["t.a"]),
                ("t.c", &["t.b"]),
                ("t.d", &["t.a"]),
            ],
            &[],
            &store,
        )
        .await;
        for module in ["t.b", "t.c"] {
            assert_eq!(
                kind(&h.status(module).state),
                "Blocked(t.a, DependencyConfigUnavailable)",
                "{module}"
            );
            assert_eq!(count(&h.probe(module).created), 0);
        }
        let recover = h
            .manager
            .plan(ChangeAction::Recover, &id("t.b"))
            .expect("plan");
        assert!(!recover.is_applicable());
        let enable = h
            .manager
            .plan(ChangeAction::Enable, &id("t.d"))
            .expect("plan");
        assert_eq!(enable.blockers[0].id, id("t.a"));
        assert!(matches!(
            h.manager
                .apply(ChangeAction::Recover, id("t.b"), confirmed(&[]))
                .await,
            Ok(ApplyOutcome::NotApplicable(_))
        ));
        // Disabling a blocked dependent is allowed and leaves t.a alone.
        h.confirm(ChangeAction::Disable, "t.b").await;
        assert_eq!(
            store.batches(),
            [vec![("t.c".into(), false), ("t.b".into(), false)]]
        );
        assert!(is_unavailable(&h.status("t.a")));
    });
}

/// 21. The record that could not be read is never written over.
#[test]
fn an_unavailable_record_is_never_written() {
    run(async {
        let store = TestStore::default();
        store.put("t.a", true, json!({"keep": 1}));
        store.fail_load("t.a");
        let h = started(
            &[("t.a", &[]), ("t.b", &[]), ("t.c", &["t.a"])],
            &[],
            &store,
        )
        .await;
        h.confirm(ChangeAction::Enable, "t.b").await;
        h.manager
            .update_settings(id("t.b"), json!({"n": 1}))
            .await
            .expect("update");
        h.confirm(ChangeAction::Disable, "t.b").await;
        let _ = h.manager.update_settings(id("t.a"), json!({})).await;
        let _ = h
            .manager
            .apply(ChangeAction::Enable, id("t.c"), confirmed(&["t.a"]))
            .await;
        assert!(
            store
                .batches()
                .iter()
                .flatten()
                .all(|(module, _)| module != "t.a"),
            "{:?}",
            store.batches()
        );
        assert_eq!(store.record("t.a"), record(true, json!({"keep": 1})));
    });
}

/// 28 (MM-5A form). There is no recovery within the run; the next run reads
/// the record again.
#[test]
fn the_next_run_reads_the_record_again() {
    run(async {
        let store = TestStore::default();
        store.put("t.a", true, json!({"keep": 1}));
        store.fail_load("t.a");
        let first = started(&[("t.a", &[])], &[], &store).await;
        assert!(is_unavailable(&first.status("t.a")));
        first.manager.shutdown().await.expect("exit");

        store.clear_failures();
        let next = started(&[("t.a", &[])], &[], &store).await;
        assert_eq!(next.status("t.a").enabled, Some(true));
        assert_eq!(next.status("t.a").state, ModuleState::Running);
        assert_eq!(next.probe("t.a").settings_seen(), [json!({"keep": 1})]);
    });
}

// ---------------------------------------------------------------------------
// Invalid settings and defaults
// ---------------------------------------------------------------------------

/// 29–32. Invalid settings fail an enabled module at startup (no instance,
///        dependents blocked) and are only logged for a disabled one; the
///        same for invalid defaults.
#[test]
fn invalid_settings_fail_only_enabled_modules_at_startup() {
    let logs = capture_logs(|| {
        run(async {
            let store = TestStore::default();
            store.put("t.a", true, json!({"invalid": 1}));
            store.put("t.b", true, json!({}));
            store.put("t.c", false, json!({"invalid": 1}));
            let h = Harness::spawn(
                &[
                    ("t.a", &[]),
                    ("t.b", &["t.a"]),
                    ("t.c", &[]),
                    ("t.d", &[]),
                    ("t.e", &[]),
                ],
                &["t.d"],
                store.clone(),
            );
            for module in ["t.d", "t.e"] {
                h.probe(module)
                    .factory(|factory| factory.defaults = json!({"invalid": true}));
            }
            h.ready().await;
            for module in ["t.a", "t.d"] {
                let status = h.status(module);
                assert_eq!(status.enabled, Some(true));
                assert_eq!(kind(&status.state), "Failed(Settings)", "{module}");
                assert_eq!(
                    failed_error(&status),
                    format!("{}: {FAKE_INVALID}", text::INVALID_SETTINGS)
                );
                assert_eq!(count(&h.probe(module).created), 0, "{module}");
                assert!(!h.manager.get_settings(&id(module)).expect("settings").valid);
            }
            assert_eq!(
                kind(&h.status("t.b").state),
                "Blocked(t.a, DependencyFailed)"
            );
            for module in ["t.c", "t.e"] {
                assert_eq!(h.status(module), ModuleStatus::default(), "{module}");
                assert!(!h.manager.get_settings(&id(module)).expect("settings").valid);
            }
        });
    });
    assert_eq!(
        logs.matches("settings of a disabled module are invalid")
            .count(),
        2,
        "{logs}"
    );
}

/// 33 (extended). The built-in catalog: valid graph and valid defaults.
#[test]
fn the_built_in_catalog_has_valid_defaults() {
    for factory in super::catalog() {
        let defaults = factory.default_settings();
        assert_eq!(super::config::check_settings(&defaults), Ok(()));
        assert_eq!(factory.validate_settings(&defaults), Ok(()));
    }
}

// ---------------------------------------------------------------------------
// Settings updates and `restart_required`
// ---------------------------------------------------------------------------

/// 34–35. Rejected settings change nothing: not the store, not the
///        snapshot, no event. The reason names the problem, not the value.
#[test]
fn rejected_settings_change_nothing() {
    run(async {
        let mut h = harness(&[("t.a", &[])]).await;
        let mut deep = json!({});
        for _ in 0..MAX_SETTINGS_DEPTH {
            deep = json!({ "n": deep });
        }
        let huge = json!({ "s": "x".repeat(MAX_SETTINGS_BYTES) });
        let cases = [
            (json!({"invalid": "hunter2-value"}), FAKE_INVALID.to_owned()),
            (json!([1, 2]), SettingsProblem::NotAnObject.to_string()),
            (huge, SettingsProblem::TooLarge.to_string()),
            (deep, SettingsProblem::TooDeep.to_string()),
            (
                json!({"telegram": {"bot_token": "hunter2-value"}}),
                SettingsProblem::SecretKey {
                    path: "telegram.bot_token".into(),
                }
                .to_string(),
            ),
        ];
        for (value, reason) in cases {
            let refused = h.manager.update_settings(id("t.a"), value).await;
            assert_eq!(
                refused,
                Err(ManagerError::Module(ModuleError::InvalidSettings {
                    module: id("t.a"),
                    reason
                }))
            );
            assert!(!format!("{refused:?}").contains("hunter2-value"));
        }
        assert!(h.store.batches().is_empty());
        assert!(h.store.records().is_empty());
        assert_eq!(settings_of(&h, "t.a"), json!({}));
        assert!(h.events().is_empty());
    });
}

/// 36–37. An update is saved as one record with the current decision and
///        announced without values; the same value again changes nothing.
#[test]
fn a_settings_update_is_saved_and_announced_once() {
    run(async {
        let mut h = harness(&[("t.a", &[])]).await;
        h.manager
            .update_settings(id("t.a"), json!({"level": 2}))
            .await
            .expect("update");
        h.manager
            .update_settings(id("t.a"), json!({"level": 2}))
            .await
            .expect("same value");
        assert_eq!(h.store.batches(), [vec![("t.a".into(), false)]]);
        assert_eq!(h.store.record("t.a"), record(false, json!({"level": 2})));
        assert_eq!(h.settings_events(), [("t.a".into(), false)]);
        let settings = h.manager.get_settings(&id("t.a")).expect("settings");
        assert_eq!(*settings.value, json!({"level": 2}));
        assert!(settings.valid && !settings.restart_required);
        assert_eq!(h.status("t.a"), ModuleStatus::default());
    });
}

/// 38–42. A running module gets new settings only in a new instance, after a
///        confirmed restart; `restart_required` shows the difference and is
///        cleared when the instance goes or the old value returns; nothing
///        restarts on its own.
#[test]
fn new_settings_apply_only_after_a_confirmed_restart() {
    run(async {
        let store = TestStore::default();
        store.put("t.a", true, json!({"level": 1}));
        let mut h = started(&[("t.a", &[])], &[], &store).await;
        h.events();
        let probe = Arc::clone(h.probe("t.a"));
        assert_eq!(
            probe.settings_seen(),
            [json!({"level": 1})],
            "from initialize"
        );

        h.manager
            .update_settings(id("t.a"), json!({"level": 2}))
            .await
            .expect("update");
        assert_eq!(h.settings_events(), [("t.a".into(), true)]);
        assert!(
            h.manager
                .get_settings(&id("t.a"))
                .expect("settings")
                .restart_required
        );
        for _ in 0..10 {
            settle().await;
        }
        assert!(h.changes().is_empty(), "not restarted on its own");
        assert_eq!(count(&probe.created), 1);

        // Back to the old value: no restart needed.
        h.manager
            .update_settings(id("t.a"), json!({"level": 1}))
            .await
            .expect("update");
        assert_eq!(h.settings_events(), [("t.a".into(), false)]);

        h.manager
            .update_settings(id("t.a"), json!({"level": 3}))
            .await
            .expect("update");
        h.events();
        h.confirm(ChangeAction::Restart, "t.a").await;
        assert_eq!(
            probe.settings_seen(),
            [json!({"level": 1}), json!({"level": 3})]
        );
        let settings = h.manager.get_settings(&id("t.a")).expect("settings");
        assert!(!settings.restart_required);

        // Cleared when the instance goes: disable, and a failure.
        h.manager
            .update_settings(id("t.a"), json!({"level": 4}))
            .await
            .expect("update");
        assert!(probe.end_background(TaskEnd::Fail));
        settle().await;
        assert_eq!(kind(&h.status("t.a").state), "Failed(Run)");
        assert!(
            !h.manager
                .get_settings(&id("t.a"))
                .expect("settings")
                .restart_required
        );
        assert_eq!(
            count(&probe.created),
            2,
            "the failure is not retried either"
        );
    });
}

/// 41. `restart_required` is cleared together with the switch to
///     `Stopping` (event order of the plan, ADR-015).
#[test]
fn restart_required_is_cleared_when_the_instance_stops() {
    run(async {
        let store = TestStore::default();
        store.put("t.a", true, json!({"level": 1}));
        let mut h = started(&[("t.a", &[])], &[], &store).await;
        h.events();
        h.manager
            .update_settings(id("t.a"), json!({"level": 2}))
            .await
            .expect("update");
        h.events();
        h.confirm(ChangeAction::Disable, "t.a").await;
        let mut expected = stop_cycle("t.a", true);
        expected.push(change("t.a", false, "Stopped"));
        assert_eq!(h.changes(), expected);
        let settings = h.manager.get_settings(&id("t.a")).expect("settings");
        assert!(!settings.restart_required);
        assert_eq!(h.store.record("t.a"), record(false, json!({"level": 2})));
    });
}

// ---------------------------------------------------------------------------
// Decisions of plans: one atomic batch
// ---------------------------------------------------------------------------

/// 43. Each plan saves its decisions as one batch before anything happens;
///     plans without decisions write nothing.
#[test]
fn plans_save_their_decisions_in_one_batch() {
    run(async {
        let h = harness(&[("t.a", &[]), ("t.b", &["t.a"]), ("t.c", &["t.b"])]).await;
        h.confirm(ChangeAction::Enable, "t.c").await;
        h.confirm(ChangeAction::Restart, "t.a").await;
        assert!(h.probe("t.c").end_background(TaskEnd::Fail));
        settle().await;
        h.confirm(ChangeAction::Recover, "t.c").await;
        h.confirm(ChangeAction::Disable, "t.a").await;
        let pair = |module: &str, enabled| (module.to_owned(), enabled);
        assert_eq!(
            h.store.batches(),
            [
                vec![pair("t.a", true), pair("t.b", true), pair("t.c", true)],
                vec![pair("t.c", false), pair("t.b", false), pair("t.a", false)],
            ],
            "restart and recover without `Enable` steps write nothing"
        );
        for module in ["t.a", "t.b", "t.c"] {
            assert_eq!(h.store.record(module), record(false, json!({})));
        }
    });
}

/// 44. A failed write changes nothing: no status, no instance, no event.
#[test]
fn a_failed_batch_changes_nothing() {
    let logs = capture_logs(|| {
        run(async {
            let mut h = harness(&[("t.a", &[]), ("t.b", &["t.a"])]).await;
            h.store.fail_saves(true);
            assert_eq!(
                h.manager
                    .apply(ChangeAction::Enable, id("t.b"), confirmed(&["t.a"]))
                    .await,
                Err(ManagerError::Store)
            );
            assert_eq!(
                h.manager.update_settings(id("t.a"), json!({"x": 1})).await,
                Err(ManagerError::Store)
            );
            assert!(h.events().is_empty());
            assert!(h.store.records().is_empty());
            assert_eq!(settings_of(&h, "t.a"), json!({}));
            for module in ["t.a", "t.b"] {
                assert_eq!(h.status(module), ModuleStatus::default());
                assert_eq!(count(&h.probe(module).created), 0);
            }
        });
    });
    assert!(logs.contains("module configuration not saved"), "{logs}");
}

/// 45. A plan that would start a module with invalid settings is refused up
///     front, before anything is saved.
#[test]
fn plans_that_would_start_unusable_settings_are_refused_up_front() {
    run(async {
        let store = TestStore::default();
        store.put("t.a", false, json!({"invalid": 1}));
        let mut h = started(&[("t.a", &[]), ("t.b", &["t.a"])], &[], &store).await;
        h.events();
        let invalid = Err(ManagerError::Module(ModuleError::InvalidSettings {
            module: id("t.a"),
            reason: FAKE_INVALID.into(),
        }));
        assert_eq!(
            h.manager
                .apply(ChangeAction::Enable, id("t.a"), confirmed(&[]))
                .await,
            invalid
        );
        assert_eq!(
            h.manager
                .apply(ChangeAction::Enable, id("t.b"), confirmed(&["t.a"]))
                .await,
            invalid,
            "also when it would start as a dependency"
        );
        assert!(store.batches().is_empty());
        assert!(h.events().is_empty());
        // After a valid update the module can be enabled.
        h.manager
            .update_settings(id("t.a"), json!({}))
            .await
            .expect("update");
        h.confirm(ChangeAction::Enable, "t.a").await;
        assert_eq!(h.status("t.a").state, ModuleState::Running);
    });
}

// ---------------------------------------------------------------------------
// Factory isolation
// ---------------------------------------------------------------------------

/// 46–48. Panics in `create`, `default_settings` and `validate_settings` are
///        caught: the module fails or the settings are refused, and the
///        manager goes on. Details stay in the log.
#[test]
fn factory_panics_are_isolated() {
    let logs = capture_logs(|| {
        run(async {
            let store = TestStore::default();
            store.put("t.c", true, json!({}));
            let h = Harness::spawn(
                &[("t.a", &[]), ("t.b", &[]), ("t.c", &[]), ("t.d", &[])],
                &["t.b"],
                store.clone(),
            );
            h.probe("t.a")
                .factory(|factory| factory.create_panic = true);
            h.probe("t.b")
                .factory(|factory| factory.defaults_panic = true);
            h.probe("t.c")
                .factory(|factory| factory.validator = Validator::Panic);
            h.ready().await;

            // `create`: Failed { Initialize } with the safe text.
            let report = h.confirm(ChangeAction::Enable, "t.a").await;
            assert_eq!(report.failed[0].error, text::PANICKED);
            let a = h.status("t.a");
            assert_eq!(kind(&a.state), "Failed(Initialize)");
            assert_eq!(failed_error(&a), text::PANICKED);

            // `default_settings`: the defaults are invalid.
            let b = h.status("t.b");
            assert_eq!(kind(&b.state), "Failed(Settings)");
            assert!(failed_error(&b).contains(text::DEFAULTS_PANICKED));

            // `validate_settings`: refused at startup and on update.
            let c = h.status("t.c");
            assert_eq!(kind(&c.state), "Failed(Settings)");
            assert!(failed_error(&c).contains(text::VALIDATOR_PANICKED));
            assert_eq!(
                h.manager.update_settings(id("t.c"), json!({})).await,
                Err(ManagerError::Module(ModuleError::InvalidSettings {
                    module: id("t.c"),
                    reason: text::VALIDATOR_PANICKED.into()
                }))
            );

            // The manager keeps working.
            h.confirm(ChangeAction::Enable, "t.d").await;
            assert_eq!(h.status("t.d").state, ModuleState::Running);
            for module in ["t.a", "t.b", "t.c"] {
                assert!(!failed_error(&h.status(module)).contains("/secret/path"));
            }
        });
    });
    for path in ["create.rs", "defaults.rs", "validate.rs"] {
        assert!(logs.contains(path), "{path} not in the log:\n{logs}");
    }
}

// ---------------------------------------------------------------------------
// Exit (ADR-015 exit mode on top of MM-4 and the PR #20 fixes)
// ---------------------------------------------------------------------------

/// 49. The exit does not wait behind queued commands; they are refused.
#[test]
fn shutdown_does_not_wait_behind_queued_commands() {
    run(async {
        let h = harness(&[("t.a", &[]), ("t.b", &[]), ("t.c", &[])]).await;
        h.confirm(ChangeAction::Enable, "t.a").await;
        h.probe("t.b")
            .script(|script| script.initialize = Behavior::Hang);
        let m = h.manager.clone();
        let long = tokio::spawn(async move {
            m.apply(ChangeAction::Enable, id("t.b"), confirmed(&[]))
                .await
        });
        let probe_b = Arc::clone(h.probe("t.b"));
        wait_until_with_pool(move || count(&probe_b.initialized) == 1).await;
        let m = h.manager.clone();
        let queued_enable = tokio::spawn(async move {
            m.apply(ChangeAction::Enable, id("t.c"), confirmed(&[]))
                .await
        });
        let m = h.manager.clone();
        let queued_update =
            tokio::spawn(async move { m.update_settings(id("t.a"), json!({"x": 1})).await });
        tokio::task::yield_now().await;

        let begun = Instant::now();
        let report = h.manager.shutdown().await.expect("exit");
        assert!(begun.elapsed() < INITIALIZE_TIMEOUT);
        assert_eq!(report.stopped, [id("t.a")]);
        let Ok(ApplyOutcome::Applied(applied)) = long.await.expect("task") else {
            panic!("the command in progress ends with its report");
        };
        assert_eq!(applied.failed[0].error, text::INTERRUPTED);
        assert_eq!(
            queued_enable.await.expect("task"),
            Err(ManagerError::ShutDown)
        );
        assert_eq!(
            queued_update.await.expect("task"),
            Err(ManagerError::ShutDown)
        );
        assert_eq!(count(&h.probe("t.c").created), 0);
        assert_eq!(
            h.store.batches().len(),
            2,
            "only the two enables before the exit"
        );
        assert_eq!(settings_of(&h, "t.a"), json!({}));
    });
}

/// 70a. A command that arrives in the same moment as the exit request is
///      not executed (the queue is biased towards a ready message, so the
///      actor checks the exit again after receiving).
#[test]
fn a_command_received_together_with_the_exit_request_is_not_executed() {
    run(async {
        let mut h = harness(&[("t.a", &[])]).await;
        let mut enable = Box::pin(
            h.manager
                .apply(ChangeAction::Enable, id("t.a"), confirmed(&[])),
        );
        assert!(
            poll_once(enable.as_mut()).is_pending(),
            "the command is sent"
        );
        let report = h.manager.shutdown().await.expect("exit");
        assert_eq!(enable.await, Err(ManagerError::ShutDown));
        assert_eq!(report, StopReport::default());
        assert_eq!(count(&h.probe("t.a").created), 0);
        assert!(h.store.batches().is_empty());
        assert!(h.events().is_empty());
    });
}

/// 51. An exit during the handling of a failure: the stop of the failed
///     module is completed, its dependents are left to `shutdown_all`.
#[test]
fn an_exit_request_stops_failure_handling_after_the_current_stop() {
    let logs = capture_logs(|| {
        run(async {
            let mut h = harness(&[("t.r", &[]), ("t.d1", &["t.r"]), ("t.d2", &["t.r"])]).await;
            h.confirm(ChangeAction::Enable, "t.d1").await;
            h.confirm(ChangeAction::Enable, "t.d2").await;
            h.changes();
            h.probe("t.r")
                .script(|script| script.stop = Behavior::Delay(Duration::from_secs(1)));
            let begun = Instant::now();
            assert!(h.probe("t.r").end_background(TaskEnd::Fail));
            let probe_r = Arc::clone(h.probe("t.r"));
            wait_until_with_pool(move || count(&probe_r.stopped) == 1).await;
            let report = h.manager.shutdown().await.expect("exit");
            assert!(begun.elapsed() <= SHUTDOWN_TIMEOUT);
            let r = h.status("t.r");
            assert_eq!(
                (r.enabled, kind(&r.state)),
                (Some(true), "Failed(Run)".into())
            );
            assert_eq!(report.stopped, [id("t.d2"), id("t.d1")]);
            for module in ["t.d1", "t.d2"] {
                assert_eq!(
                    h.status(module),
                    ModuleStatus {
                        enabled: Some(true),
                        state: ModuleState::Stopped
                    },
                    "{module}: stopped by the exit, not blocked"
                );
                assert_eq!(count(&h.probe(module).stopped), 1);
            }
        });
    });
    let line = log_line(&logs, "failure handling interrupted");
    assert!(line.contains("dependents=2"), "{line}");
}

/// 52. An exit during the cascade: only the dependent being stopped is
///     finished (blocked or failed by its own stop); the rest go to
///     `shutdown_all`.
#[test]
fn an_exit_request_during_the_cascade_finishes_only_the_current_dependent() {
    for (behavior, expected) in [
        (
            Behavior::Delay(Duration::from_secs(1)),
            "Blocked(t.r, DependencyFailed)",
        ),
        (Behavior::FailAfter(Duration::from_secs(1)), "Failed(Stop)"),
        (Behavior::Hang, "Failed(Stop)"),
    ] {
        run(async {
            let h = harness(&[("t.r", &[]), ("t.d1", &["t.r"]), ("t.d2", &["t.d1"])]).await;
            h.confirm(ChangeAction::Enable, "t.d2").await;
            h.probe("t.d2").script(|script| script.stop = behavior);
            assert!(h.probe("t.r").end_background(TaskEnd::Fail));
            let probe = Arc::clone(h.probe("t.d2"));
            wait_until_with_pool(move || count(&probe.stopped) == 1).await;
            let begun = Instant::now();
            let report = h.manager.shutdown().await.expect("exit");
            let d2 = h.status("t.d2");
            assert_eq!(kind(&d2.state), expected, "{behavior:?}");
            match behavior {
                Behavior::Hang => {
                    assert_eq!(failed_error(&d2), text::TIMED_OUT);
                    assert_eq!(begun.elapsed(), STOP_TIMEOUT);
                }
                Behavior::FailAfter(_) => assert_eq!(failed_error(&d2), "fake stop error"),
                _ => {}
            }
            assert_eq!(report.stopped, [id("t.d1")], "{behavior:?}");
            assert_eq!(h.status("t.d1").state, ModuleState::Stopped);
            assert_eq!(kind(&h.status("t.r").state), "Failed(Run)");
        });
    }
}

/// 54. The same rule on the common path of a health check panic.
#[test]
fn a_health_check_panic_during_the_exit_does_not_cascade() {
    run(async {
        let h = harness(&[("t.a", &[]), ("t.b", &["t.a"])]).await;
        h.confirm(ChangeAction::Enable, "t.b").await;
        h.probe("t.a").script(|script| {
            script.health = HealthBehavior::Late {
                after: Duration::from_secs(1),
                panic: true,
            };
            script.background = Background::IgnoresCancellation;
        });
        // The new instance of t.a has a task that ignores cancellation, so
        // releasing it takes `TASKS_STOP_TIMEOUT`.
        h.confirm(ChangeAction::Restart, "t.a").await;
        let m = h.manager.clone();
        let check = tokio::spawn(async move { m.check_health(id("t.a")).await });
        // The check panics after 1 s; releasing the instance then waits
        // `TASKS_STOP_TIMEOUT` for the task: the exit comes in between.
        tokio::time::sleep(Duration::from_millis(1500)).await;
        let report = h.manager.shutdown().await.expect("exit");
        assert!(check.await.expect("task").is_ok());
        assert_eq!(kind(&h.status("t.a").state), "Failed(HealthCheck)");
        assert_eq!(
            h.status("t.b").state,
            ModuleState::Stopped,
            "stopped by the exit, not blocked"
        );
        assert_eq!(report.stopped, [id("t.b")]);
    });
}

/// 55. A write still waiting at the exit deadline is given up: an error, and
///     nothing changed.
#[test]
fn a_store_write_cut_by_the_exit_deadline_changes_nothing() {
    run(async {
        let mut h = harness(&[("t.a", &[])]).await;
        h.store.hang_saves(true);
        let m = h.manager.clone();
        let enable = tokio::spawn(async move {
            m.apply(ChangeAction::Enable, id("t.a"), confirmed(&[]))
                .await
        });
        let store = h.store.clone();
        wait_until_with_pool(move || store.batches().len() == 1).await;
        let begun = Instant::now();
        let report = h.manager.shutdown().await.expect("exit");
        assert_eq!(
            begun.elapsed(),
            SHUTDOWN_TIMEOUT,
            "the write waits until the deadline"
        );
        assert_eq!(enable.await.expect("task"), Err(ManagerError::Store));
        assert_eq!(report, StopReport::default());
        assert_eq!(h.status("t.a"), ModuleStatus::default());
        assert_eq!(count(&h.probe("t.a").created), 0);
        assert!(h.store.records().is_empty());
        assert!(h.events().is_empty());
    });
}

/// 56. The overall exit deadline holds with a hung store and a hung module.
#[test]
fn the_exit_deadline_holds_with_a_slow_store_and_hung_modules() {
    run(async {
        let h = harness(&[("t.a", &[]), ("t.b", &[])]).await;
        h.confirm(ChangeAction::Enable, "t.a").await;
        h.probe("t.a").script(|script| script.stop = Behavior::Hang);
        h.store.hang_saves(true);
        let m = h.manager.clone();
        let enable = tokio::spawn(async move {
            m.apply(ChangeAction::Enable, id("t.b"), confirmed(&[]))
                .await
        });
        tokio::task::yield_now().await;
        let begun = Instant::now();
        let report = h.manager.shutdown().await.expect("exit");
        assert_eq!(begun.elapsed(), SHUTDOWN_TIMEOUT);
        assert_eq!(enable.await.expect("task"), Err(ManagerError::Store));
        assert_eq!(report.failed[0].error, text::SHUTDOWN_DEADLINE);
        assert_eq!(kind(&h.status("t.a").state), "Failed(Stop)");
    });
}

/// 58 (extended). After the reply to `Shutdown` nothing is published: not
/// after a normal preparation, a late failure report or a repeated request.
#[test]
fn nothing_is_published_after_the_shutdown_reply() {
    run(async {
        let store = TestStore::default();
        store.put("t.a", true, json!({}));
        let mut h = Harness::spawn(&[("t.a", &[])], &[], store.clone());
        h.probe("t.a")
            .script(|script| script.background = Background::FailsOnCancel);
        h.ready().await;
        h.events();
        let first = h.manager.shutdown().await;
        assert!(first.is_ok());
        let events = h.events();
        assert_eq!(events.len(), 2, "only the stop of t.a: {events:?}");
        assert_eq!(h.manager.shutdown().await, Err(ManagerError::ShutDown));
        settle().await;
        assert!(h.events().is_empty());
    });
}

// ---------------------------------------------------------------------------
// Logs, order, consistency
// ---------------------------------------------------------------------------

/// 59. Settings values never reach the log, the events or `Debug`.
#[test]
fn settings_values_never_reach_the_log_or_events() {
    const PRIVATE: &str = "PRIVATE-VALUE-7731";
    let mut seen_events = String::new();
    let logs = capture_logs(|| {
        run(async {
            let store = TestStore::default();
            store.put("t.a", true, json!({"note": PRIVATE}));
            let mut h = started(&[("t.a", &[])], &[], &store).await;
            h.manager
                .update_settings(id("t.a"), json!({"note": format!("{PRIVATE}-2")}))
                .await
                .expect("update");
            let _ = h
                .manager
                .update_settings(id("t.a"), json!({"note": PRIVATE, "invalid": PRIVATE}))
                .await;
            let settings = h.manager.get_settings(&id("t.a")).expect("settings");
            seen_events.push_str(&format!("{settings:?} {:?}", h.events()));
            seen_events.push_str(&format!("{:?}", store.record("t.a")));
        });
    });
    assert!(!logs.contains(PRIVATE), "{logs}");
    assert!(!seen_events.contains(PRIVATE), "{seen_events}");
}

/// 60–61. Events follow the documented order; settings updates and plans run
///        in the order they arrive.
#[test]
fn settings_updates_and_plans_run_in_arrival_order() {
    run(async {
        let store = TestStore::default();
        store.put("t.a", true, json!({"v": 0}));
        let mut h = started(&[("t.a", &[])], &[], &store).await;
        h.events();
        let m = h.manager.clone();
        let first =
            tokio::spawn(async move { m.update_settings(id("t.a"), json!({"v": 1})).await });
        let m = h.manager.clone();
        let restart = tokio::spawn(async move {
            m.apply(ChangeAction::Restart, id("t.a"), confirmed(&[]))
                .await
        });
        let m = h.manager.clone();
        let second =
            tokio::spawn(async move { m.update_settings(id("t.a"), json!({"v": 2})).await });
        assert_eq!(first.await.expect("task"), Ok(()));
        assert!(matches!(
            restart.await.expect("task"),
            Ok(ApplyOutcome::Applied(_))
        ));
        assert_eq!(second.await.expect("task"), Ok(()));
        let events = h.events();
        let settings_changed = |event: &ModuleEvent, rr| {
            *event
                == ModuleEvent::SettingsChanged {
                    id: id("t.a"),
                    restart_required: rr,
                }
        };
        assert!(settings_changed(&events[0], true), "{events:?}");
        assert!(is_change(&events[1], "t.a", "Stopping"));
        assert!(is_change(&events[4], "t.a", "Starting"));
        assert!(is_change(&events[5], "t.a", "Running"));
        assert!(settings_changed(&events[6], true), "{events:?}");
        assert_eq!(events.len(), 7);
        assert_eq!(
            h.probe("t.a").settings_seen(),
            [json!({"v": 0}), json!({"v": 1})]
        );
    });
}

/// 62. After every command the store agrees with the statuses: a stored
///     record holds the published decision and the current settings.
#[test]
fn the_store_matches_the_statuses_after_every_command() {
    run(async {
        let store = TestStore::default();
        store.put("t.b", true, json!({"x": 1}));
        let mut h = started(
            &[
                ("t.a", &[]),
                ("t.b", &["t.a"]),
                ("t.c", &["t.b"]),
                ("t.d", &[]),
            ],
            &["t.a"],
            &store,
        )
        .await;
        let check = |h: &mut Harness| {
            h.events();
            for (module, status) in h.manager.statuses().expect("ready") {
                let settings = h.manager.get_settings(&module).expect("settings");
                match h.store.record(module.as_str()) {
                    Some(record) => {
                        assert_eq!(Some(record.enabled), status.enabled, "{module}");
                        assert_eq!(record.settings, *settings.value, "{module}");
                    }
                    None => assert_eq!(*settings.value, json!({}), "{module}: defaults"),
                }
            }
        };
        check(&mut h);
        h.confirm(ChangeAction::Enable, "t.c").await;
        check(&mut h);
        h.manager
            .update_settings(id("t.c"), json!({"y": 2}))
            .await
            .expect("update");
        check(&mut h);
        h.confirm(ChangeAction::Restart, "t.b").await;
        check(&mut h);
        h.confirm(ChangeAction::Disable, "t.b").await;
        check(&mut h);
        h.confirm(ChangeAction::Enable, "t.d").await;
        check(&mut h);
        assert!(h.probe("t.a").end_background(TaskEnd::Fail));
        settle().await;
        check(&mut h);
        h.confirm(ChangeAction::Recover, "t.a").await;
        check(&mut h);
        h.manager.shutdown().await.expect("exit");
        check(&mut h);
    });
}

/// 63 (without `replace_config`). Errors of the settings commands.
#[test]
fn settings_commands_report_unknown_modules_and_the_exit() {
    run(async {
        let h = harness(&[("t.a", &[])]).await;
        let unknown_module = Err(ManagerError::Module(ModuleError::UnknownModule(id("t.zz"))));
        assert_eq!(h.manager.get_settings(&id("t.zz")), unknown_module);
        assert_eq!(
            h.manager.update_settings(id("t.zz"), json!({})).await,
            Err(ManagerError::Module(ModuleError::UnknownModule(id("t.zz"))))
        );
        h.manager.shutdown().await.expect("exit");
        assert_eq!(
            h.manager.update_settings(id("t.a"), json!({"x": 1})).await,
            Err(ManagerError::ShutDown)
        );
        assert_eq!(settings_of(&h, "t.a"), json!({}), "readable after the exit");
    });
}

// ---------------------------------------------------------------------------
// Interruption: MM-4 behaviour kept (regression tests)
// ---------------------------------------------------------------------------

/// 64. A `start` interrupted by the exit fails with the start phase; its
///     background task is aborted after `TASKS_STOP_TIMEOUT`; the rest of
///     the plan is skipped and no dependent is blocked.
#[test]
fn a_start_interrupted_by_the_exit_fails_with_the_start_phase() {
    let logs = capture_logs(|| {
        run(async {
            let h = harness(&[("t.a", &[]), ("t.b", &["t.a"])]).await;
            h.probe("t.a").script(|script| {
                script.start = Behavior::Hang;
                script.background = Background::IgnoresCancellation;
            });
            let m = h.manager.clone();
            let enable = tokio::spawn(async move {
                m.apply(ChangeAction::Enable, id("t.b"), confirmed(&["t.a"]))
                    .await
            });
            let probe = Arc::clone(h.probe("t.a"));
            wait_until_with_pool(move || count(&probe.started) == 1).await;
            let begun = Instant::now();
            h.manager.shutdown().await.expect("exit");
            assert_eq!(begun.elapsed(), TASKS_STOP_TIMEOUT);
            let Ok(ApplyOutcome::Applied(report)) = enable.await.expect("task") else {
                panic!("applied");
            };
            assert_eq!(report.failed[0].error, text::INTERRUPTED);
            assert!(report.interrupted);
            let a = h.status("t.a");
            assert_eq!(kind(&a.state), "Failed(Start)");
            assert_eq!(failed_error(&a), text::INTERRUPTED);
            assert_eq!(count(&h.probe("t.a").stopped), 0);
            assert_eq!(h.status("t.b"), ModuleStatus::default(), "not blocked");
            settle().await;
            let probe = h.probe("t.a");
            assert_eq!(count(&probe.background), 0);
            assert_eq!(count(&probe.dropped), count(&probe.created));
        });
    });
    assert!(logs.contains("background tasks of a failed instance were aborted"));
}

/// 65. An exit requested right after `initialize` fails the start phase.
#[test]
fn an_exit_right_after_initialize_fails_the_start_phase() {
    run(async {
        let h = harness(&[("t.a", &[])]).await;
        let m = h.manager.clone();
        h.probe("t.a").on_initialize(move || {
            // Requests the exit and does not wait for it.
            let _ = poll_once(m.shutdown());
        });
        let outcome = h
            .manager
            .apply(ChangeAction::Enable, id("t.a"), confirmed(&[]))
            .await;
        assert!(matches!(outcome, Ok(ApplyOutcome::Applied(_))));
        let a = h.status("t.a");
        assert_eq!(kind(&a.state), "Failed(Start)");
        assert_eq!(failed_error(&a), text::INTERRUPTED);
        assert_eq!(count(&h.probe("t.a").stopped), 0);
    });
}

/// 69. An exit during a health check leaves the instance to `shutdown_all`.
#[test]
fn an_exit_during_a_health_check_leaves_the_instance_to_shutdown_all() {
    for (health, expected) in [
        (HealthBehavior::Hang, "Failed(Stop)"),
        (
            HealthBehavior::Late {
                after: Duration::from_secs(3),
                panic: false,
            },
            "Stopped",
        ),
    ] {
        run(async {
            let h = harness(&[("t.a", &[])]).await;
            h.confirm(ChangeAction::Enable, "t.a").await;
            h.probe("t.a")
                .script(|script| script.health = health.clone());
            let m = h.manager.clone();
            let check = tokio::spawn(async move { m.check_health(id("t.a")).await });
            tokio::time::sleep(Duration::from_secs(1)).await;
            assert_eq!(h.status("t.a").state, ModuleState::Running);
            h.manager.shutdown().await.expect("exit");
            assert_eq!(check.await.expect("task"), Err(ManagerError::ShutDown));
            let a = h.status("t.a");
            assert_eq!(kind(&a.state), expected, "{health:?}");
            if expected == "Stopped" {
                assert_eq!(count(&h.probe("t.a").stopped), 1);
            } else {
                assert_eq!(failed_error(&a), text::UNRESPONSIVE);
            }
        });
    }
}

/// 70. Every message after the exit request follows the exit rules:
///     commands are refused without effect, reports are only logged,
///     `shutdown_all` runs once.
#[test]
fn every_message_after_the_exit_request_follows_the_exit_rules() {
    let logs = capture_logs(|| {
        run(async {
            let mut h = harness(&[("t.a", &[]), ("t.b", &[]), ("t.x", &[])]).await;
            h.confirm(ChangeAction::Enable, "t.a").await;
            h.confirm(ChangeAction::Enable, "t.b").await;
            h.events();
            h.probe("t.x")
                .script(|script| script.initialize = Behavior::Delay(Duration::from_secs(3)));
            let m = h.manager.clone();
            let long = tokio::spawn(async move {
                m.apply(ChangeAction::Enable, id("t.x"), confirmed(&[]))
                    .await
            });
            let probe = Arc::clone(h.probe("t.x"));
            wait_until_with_pool(move || count(&probe.initialized) == 1).await;
            let m = h.manager.clone();
            let apply = tokio::spawn(async move {
                m.apply(ChangeAction::Disable, id("t.a"), confirmed(&[]))
                    .await
            });
            let m = h.manager.clone();
            let health = tokio::spawn(async move { m.check_health(id("t.a")).await });
            let m = h.manager.clone();
            let update =
                tokio::spawn(async move { m.update_settings(id("t.b"), json!({"z": 1})).await });
            assert!(h.probe("t.a").end_background(TaskEnd::Fail));
            for _ in 0..20 {
                tokio::task::yield_now().await;
            }
            let m = h.manager.clone();
            let second = tokio::spawn(async move { m.shutdown().await });
            let first = h.manager.shutdown().await;
            let _ = long.await;
            assert_eq!(apply.await.expect("task"), Err(ManagerError::ShutDown));
            assert_eq!(health.await.expect("task"), Err(ManagerError::ShutDown));
            assert_eq!(update.await.expect("task"), Err(ManagerError::ShutDown));
            let outcomes = [first, second.await.expect("task")];
            assert_eq!(
                outcomes.iter().filter(|outcome| outcome.is_ok()).count(),
                1,
                "{outcomes:?}"
            );
            let events = h.events();
            assert!(
                events
                    .iter()
                    .all(|event| matches!(event, ModuleEvent::StatusChanged { .. })),
                "no settings or health events: {events:?}"
            );
            for module in ["t.a", "t.b"] {
                assert_eq!(count(&h.probe(module).stopped), 1, "{module}");
                assert_eq!(h.status(module).enabled, Some(true));
            }
            assert_eq!(settings_of(&h, "t.b"), json!({}));
            assert_eq!(h.store.batches().len(), 3, "the three enables only");
        });
    });
    assert_eq!(
        logs.matches("stopping modules: the application is exiting")
            .count(),
        1
    );
    assert!(log_line(&logs, "background task failed after the exit request").contains("WARN"));
}

#[test]
fn snapshot_and_settings_debug_hide_values() {
    run(async {
        let store = TestStore::default();
        store.put("t.a", false, json!({"note": "PRIVATE-DEBUG"}));
        let h = started(&[("t.a", &[])], &[], &store).await;
        let settings = h.manager.get_settings(&id("t.a")).expect("settings");
        assert!(!format!("{settings:?}").contains("PRIVATE-DEBUG"));
        let entry = super::manager::SettingsEntry::Loaded {
            value: Arc::new(json!({"note": "PRIVATE-DEBUG"})),
            valid: true,
        };
        assert!(!format!("{entry:?}").contains("PRIVATE-DEBUG"));
        let config = ModuleConfig {
            enabled: false,
            settings: json!({"note": "PRIVATE-DEBUG"}),
        };
        assert!(!format!("{config:?}").contains("PRIVATE-DEBUG"));
    });
}
