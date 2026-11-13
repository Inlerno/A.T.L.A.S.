//! Tests of the lifecycle manager (issue #9) on fake modules.
//!
//! Every test runs on a current-thread runtime with paused time: timeouts
//! elapse instantly and deterministically, whatever the machine speed. Every
//! published status change is checked against the status invariant
//! (decision 23) and against the previous status of the same module, and the
//! snapshot against the published changes.

use std::sync::Arc;
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use tokio::time::Instant;

use super::context::ModuleContext;
use super::contract::{
    FailurePhase, Health, ModuleError, ModuleFactory, ModuleFailure, ModuleState, ModuleStatus,
};
use super::graph::ModuleGraph;
use super::manager::{
    self, ApplyOutcome, HEALTH_CHECK_TIMEOUT, INITIALIZE_TIMEOUT, ManagerError, SHUTDOWN_TIMEOUT,
    START_TIMEOUT, STOP_TIMEOUT, StopReport, TASKS_STOP_TIMEOUT, text,
};
use super::plan::{ChangeAction, Confirmation};
use super::testing::*;
use crate::app::{AppState, ExitStep};
use crate::events::bus::{EventBus, Received};
use crate::events::types::{AppEvent, ModuleEvent};

#[test]
fn enable_goes_through_initializing_starting_running() {
    run(async {
        let mut h = harness(&[("t.a", &[])]).await;
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
        let h = harness(&[("t.a", &[])]).await;
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
        let h = harness(&[("t.a", &[])]).await;
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
            let mut h = harness(&[("t.a", &[]), ("t.b", &[])]).await;
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
            assert_eq!(status.enabled, Some(true), "a failed module stays enabled");
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

#[test]
fn panic_details_go_to_the_log_not_to_the_status() {
    let logs = capture_logs(|| {
        run(async {
            let h = harness(&[("t.a", &[]), ("t.b", &[])]).await;
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
        ])
        .await;
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
        let mut h = harness(&[("t.a", &[]), ("t.b", &["t.a"]), ("t.c", &["t.b"])]).await;
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
        ])
        .await;

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
        let mut h = harness(&[("t.a", &[]), ("t.b", &["t.a"]), ("t.c", &["t.b"])]).await;
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
        let mut h = harness(&[("t.a", &[]), ("t.b", &["t.a"]), ("t.c", &["t.b"])]).await;
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
        let mut h = harness(&[("t.a", &[]), ("t.b", &["t.a"])]).await;
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
        ])
        .await;
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
        let mut h = harness(&[("t.a", &[]), ("t.b", &["t.a"])]).await;
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
        let mut h = harness(&[("t.a", &[]), ("t.b", &["t.a"])]).await;
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
        ])
        .await;
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
        ])
        .await;
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
            assert_eq!(status.enabled, Some(false), "{module} is disabled");
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

/// A disabled module whose stop failed (`{ Some(false), Failed { Stop } }`)
/// is never started by enabling a dependent: only a confirmed `recover`
/// brings it back (ADR-012). Checks the calls, not only the statuses.
#[test]
fn enabling_a_dependent_does_not_restart_a_dependency_whose_stop_failed() {
    run(async {
        let mut h = harness(&[("t.a", &[]), ("t.b", &["t.a"])]).await;
        h.confirm(ChangeAction::Enable, "t.b").await;
        h.probe("t.a").script(|script| script.stop = Behavior::Fail);
        h.confirm(ChangeAction::Disable, "t.a").await;
        let a = h.status("t.a");
        assert_eq!(a.enabled, Some(false));
        assert_eq!(kind(&a.state), "Failed(Stop)");
        assert_eq!(
            h.status("t.b"),
            ModuleStatus::default(),
            "disabled by the cascade"
        );
        h.changes();
        let probe_a = Arc::clone(h.probe("t.a"));
        let probe_b = Arc::clone(h.probe("t.b"));
        let calls = |probe: &Probe| {
            (
                count(&probe.created),
                count(&probe.initialized),
                count(&probe.started),
            )
        };
        let (a_before, b_before) = (calls(&probe_a), calls(&probe_b));
        let batches = h.store.batches().len();

        let plan = h
            .manager
            .plan(ChangeAction::Enable, &id("t.b"))
            .expect("plan");
        assert!(!plan.is_applicable());
        assert_eq!(plan.blockers[0].id, id("t.a"));
        let outcome = h
            .apply(ChangeAction::Enable, "t.b", confirmed(&["t.a"]))
            .await;
        assert!(
            matches!(outcome, Ok(ApplyOutcome::NotApplicable(_))),
            "{outcome:?}"
        );
        assert_eq!(
            h.apply(ChangeAction::Enable, "t.a", confirmed(&[])).await,
            Err(ManagerError::Module(ModuleError::ActionNotApplicable {
                module: id("t.a"),
                action: ChangeAction::Enable
            })),
            "enable of the failed module itself does not bypass recover"
        );
        settle().await;
        assert_eq!(
            calls(&probe_a),
            a_before,
            "t.a: no new instance, initialize or start"
        );
        assert_eq!(calls(&probe_b), b_before, "t.b: not started either");
        assert!(h.changes().is_empty());
        assert_eq!(h.store.batches().len(), batches, "nothing saved");
        assert_eq!(kind(&h.status("t.a").state), "Failed(Stop)");
        assert_eq!(h.status("t.a").enabled, Some(false));

        // Only a confirmed recover brings t.a back; then t.b can be enabled.
        h.probe("t.a")
            .script(|script| script.stop = Behavior::Succeed);
        h.confirm(ChangeAction::Recover, "t.a").await;
        assert_eq!(h.status("t.a").state, ModuleState::Running);
        h.confirm(ChangeAction::Enable, "t.b").await;
        assert_eq!(h.status("t.b").state, ModuleState::Running);
    });
}

#[test]
fn a_repeated_disable_changes_nothing_and_tasks_are_finished() {
    run(async {
        let mut h = harness(&[("t.a", &[])]).await;
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
        let h = harness(&[("t.a", &[])]).await;
        h.probe("t.a")
            .script(|script| script.background = Background::IgnoresCancellation);
        h.confirm(ChangeAction::Enable, "t.a").await;
        let started = Instant::now();
        let report = h.confirm(ChangeAction::Disable, "t.a").await;
        assert_eq!(started.elapsed(), TASKS_STOP_TIMEOUT);
        assert_eq!(report.failed[0].error, text::TASKS_ABORTED);
        let status = h.status("t.a");
        assert_eq!(status.enabled, Some(false));
        assert_eq!(kind(&status.state), "Failed(Stop)");
        settle().await;
        assert_eq!(count(&h.probe("t.a").background), 0, "aborted");
    });
}

#[test]
fn health_results_are_published_and_never_change_the_state() {
    run(async {
        let mut h = harness(&[("t.a", &[])]).await;
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
        let mut h = harness(&[("t.a", &[])]).await;
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
        let h = harness(&[("t.a", &[])]).await;
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
        assert_eq!(status.enabled, Some(false));
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
        let mut h = harness(&[("t.a", &[]), ("t.b", &["t.a"])]).await;
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
        let mut h = harness(&[("t.a", &[]), ("t.b", &["t.a"])]).await;
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
        let mut h = harness(&[("t.a", &[]), ("t.b", &[])]).await;
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
        let mut h = harness(&[("t.a", &[])]).await;
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
        let mut h = harness(&[("t.a", &[]), ("t.b", &[])]).await;
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
        let mut h = harness(&[("t.a", &[]), ("t.b", &["t.a"]), ("t.c", &[])]).await;
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
        let mut h = harness(&modules).await;
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
        let h = harness(&[("t.a", &[])]).await;
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
        let mut h = harness(&[("t.a", &[]), ("t.b", &["t.a"]), ("t.c", &["t.b"])]).await;
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
    let logs = capture_logs(|| {
        run(async {
            let mut h = harness(&[("t.a", &[]), ("t.b", &[])]).await;
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
    });
    // D2: the report of t.a, which arrived while the modules were stopping,
    // is logged with its text once the queue is taken, and changed nothing.
    let line = log_line(&logs, "background task failed after the exit request");
    assert!(
        line.contains("WARN") && line.contains("module=t.a"),
        "{line}"
    );
    assert!(line.contains("panic: fake background panic"), "{line}");
}

#[test]
fn the_status_invariant_holds_through_a_long_scenario() {
    run(async {
        let mut h = harness(&[
            ("t.a", &[]),
            ("t.b", &["t.a"]),
            ("t.c", &["t.b"]),
            ("t.d", &[]),
        ])
        .await;
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
        let statuses = h.manager.statuses().expect("ready after the exit");
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
                enabled: Some(false),
                state: ModuleState::Stopped
            }
        ));
    });
}

#[test]
fn an_invalid_catalog_is_logged_and_leaves_the_manager_without_modules() {
    let logs = capture_logs(|| {
        run(async {
            let (first, _) = FakeFactory::new("t.a", &[]);
            let (second, _) = FakeFactory::new("t.a", &[]);
            let catalog: Vec<Box<dyn ModuleFactory>> = vec![Box::new(first), Box::new(second)];
            let store = Box::new(TestStore::default());
            let (manager, actor) = manager::create(catalog, store, EventBus::new(8));
            tokio::spawn(actor);
            settle().await;
            assert!(manager.is_ready());
            assert!(manager.statuses().expect("ready").is_empty());
            let outcome = manager
                .apply(ChangeAction::Enable, id("t.a"), confirmed(&[]))
                .await;
            assert!(matches!(
                outcome,
                Err(ManagerError::Module(ModuleError::UnknownModule(_)))
            ));
        });
    });
    // Decision 11: a broken catalog is never ignored silently.
    let line = logs
        .lines()
        .find(|line| line.contains("invalid module catalog"))
        .unwrap_or_else(|| panic!("no catalog error in the log:\n{logs}"));
    assert!(line.contains("ERROR"), "{line}");
    assert!(
        line.contains("duplicate module id `t.a`"),
        "the reason is logged: {line}"
    );
}

#[test]
fn the_built_in_catalog_is_valid() {
    let descriptors = super::catalog()
        .iter()
        .map(|factory| factory.descriptor().clone())
        .collect::<Vec<_>>();
    assert!(ModuleGraph::build(descriptors).is_ok());
}

// ---------------------------------------------------------------------------
// PR #20 fixes D1, D2, L1 (`claude/PR20_FIX_PLAN.md`, revision 3).
// ---------------------------------------------------------------------------

/// Values of `background` seen by the drops of `probe`'s instances.
fn background_at_drop(probe: &Probe) -> Vec<usize> {
    probe
        .background_at_drop
        .lock()
        .expect("drop record")
        .clone()
}

/// Test 77. The stop order of MM-4 that ADR-014 now describes: the token is
/// cancelled, `stop` runs, the instance is dropped, and only then are the
/// background tasks waited for and the rest aborted.
#[test]
fn the_instance_is_dropped_before_its_tasks_are_waited_for() {
    run(async {
        let mut h = harness(&[("t.a", &[])]).await;
        h.probe("t.a")
            .script(|script| script.background = Background::IgnoresCancellation);
        h.confirm(ChangeAction::Enable, "t.a").await;
        h.changes();
        let probe = Arc::clone(h.probe("t.a"));

        let started = Instant::now();
        h.confirm(ChangeAction::Disable, "t.a").await;
        assert_eq!(
            started.elapsed(),
            TASKS_STOP_TIMEOUT,
            "the tasks were waited for after stop and the drop"
        );
        assert_eq!(
            background_at_drop(&probe),
            [1],
            "the task was still alive when the instance was dropped"
        );
        assert_eq!(count(&probe.stopped), 1, "stop ran before the drop");
        let status = h.status("t.a");
        assert_eq!(status.enabled, Some(false));
        assert_eq!(kind(&status.state), "Failed(Stop)");
        assert_eq!(failed_error(&status), text::TASKS_ABORTED);
        settle().await;
        assert_eq!(count(&probe.background), 0, "then the task was aborted");
    });
}

/// Waits, in real time, for something another thread (the blocking pool)
/// does; fails after ten seconds instead of hanging.
fn wait_for_thread(mut condition: impl FnMut() -> bool) {
    let limit = std::time::Instant::now() + Duration::from_secs(10);
    while !condition() {
        assert!(
            std::time::Instant::now() < limit,
            "the other thread did not get there"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// Test 73 (D1). A `Drop` that panics after a successful `stop` does not end
/// the manager: the stop counts as failed (`Failed { Stop }` with the usual
/// text, the cause in the log), the next command runs, and at exit the other
/// modules are still stopped with `stop`.
#[test]
fn a_panicking_drop_after_stop_fails_the_stop_and_the_manager_keeps_working() {
    let logs = capture_logs(|| {
        run(async {
            let mut h = harness(&[("t.a", &[]), ("t.b", &[]), ("t.c", &[])]).await;
            for module in ["t.a", "t.b", "t.c"] {
                h.confirm(ChangeAction::Enable, module).await;
            }
            h.probe("t.a")
                .script(|script| script.drop = DropBehavior::Panic);
            h.changes();

            // disable
            let report = h.confirm(ChangeAction::Disable, "t.a").await;
            assert_eq!(report.failed.len(), 1);
            assert_eq!(report.failed[0].error, text::PANICKED);
            let status = h.status("t.a");
            assert_eq!(status.enabled, Some(false), "disable keeps enabled = false");
            assert_eq!(kind(&status.state), "Failed(Stop)");
            assert_eq!(failed_error(&status), text::PANICKED);
            assert_eq!(
                h.changes(),
                [
                    change("t.a", true, "Stopping"),
                    change("t.a", true, "Failed(Stop)"),
                    change("t.a", false, "Failed(Stop)"),
                ]
            );
            assert_eq!(count(&h.probe("t.a").dropped), 1);
            assert_eq!(count(&h.probe("t.a").drops_finished), 0);

            // the manager keeps working
            assert_eq!(h.manager.check_health(id("t.c")).await, Ok(Health::Healthy));

            // exit: t.b panics in Drop, t.c and the rest stop normally
            h.probe("t.b")
                .script(|script| script.drop = DropBehavior::Panic);
            h.changes();
            let report = h.manager.shutdown().await.expect("shutdown");
            assert_eq!(report.stopped, [id("t.c")]);
            assert_eq!(report.failed.len(), 1);
            assert_eq!(report.failed[0].id, id("t.b"));
            assert_eq!(report.failed[0].error, text::PANICKED);
            let mut expected = stop_cycle("t.c", true);
            expected.push(change("t.b", true, "Stopping"));
            expected.push(change("t.b", true, "Failed(Stop)"));
            assert_eq!(h.changes(), expected);
            for module in ["t.b", "t.c"] {
                assert_eq!(count(&h.probe(module).stopped), 1, "{module}: stop ran");
            }
            settle().await;
            assert!(h.changes().is_empty(), "nothing after the reply");
        });
    });
    let lines: Vec<&str> = logs
        .lines()
        .filter(|line| line.contains("module instance drop panicked"))
        .collect();
    assert_eq!(lines.len(), 2, "{logs}");
    for (line, module) in lines.iter().zip(["t.a", "t.b"]) {
        assert!(line.contains("ERROR"), "{line}");
        assert!(line.contains(&format!("module={module}")), "{line}");
        assert!(line.contains("call=\"drop\""), "{line}");
        assert!(line.contains("fake drop panic"), "{line}");
    }
}

/// Test 75 (D1, decision 7). A failed `initialize` whose `Drop` panics keeps
/// the module's own error as the root cause; the drop goes to the log. After
/// a failed `stop` the stop error stays, whatever the drop does.
#[test]
fn a_panicking_drop_after_a_failed_initialize_keeps_the_root_cause() {
    let logs = capture_logs(|| {
        run(async {
            let mut h = harness(&[("t.a", &[]), ("t.b", &[])]).await;
            h.probe("t.a").script(|script| {
                script.initialize = Behavior::Fail;
                script.drop = DropBehavior::Panic;
            });
            let report = h.confirm(ChangeAction::Enable, "t.a").await;
            assert_eq!(report.failed[0].error, "fake initialize error");
            let status = h.status("t.a");
            assert_eq!(kind(&status.state), "Failed(Initialize)");
            assert_eq!(failed_error(&status), "fake initialize error");
            assert_eq!(count(&h.probe("t.a").dropped), 1);

            // the manager keeps working
            h.confirm(ChangeAction::Enable, "t.b").await;
            assert_eq!(h.status("t.b").state, ModuleState::Running);

            // a failed stop and a panicking drop: the stop error stays
            h.probe("t.b").script(|script| {
                script.stop = Behavior::Fail;
                script.drop = DropBehavior::Panic;
            });
            h.changes();
            let report = h.confirm(ChangeAction::Disable, "t.b").await;
            assert_eq!(report.failed[0].error, "fake stop error");
            assert_eq!(failed_error(&h.status("t.b")), "fake stop error");
            assert_eq!(count(&h.probe("t.b").dropped), 1);
        });
    });
    let lines: Vec<&str> = logs
        .lines()
        .filter(|line| line.contains("module instance drop panicked"))
        .collect();
    assert_eq!(lines.len(), 2, "{logs}");
    assert!(lines[0].contains("module=t.a"), "{}", lines[0]);
    assert!(lines[1].contains("module=t.b"), "{}", lines[1]);
}

/// Test 75, variant (D1-9, decision 7). After a failed `initialize`,
/// `Failed { Initialize }` is published before the instance is dropped: a
/// `Drop` that blocks does not delay it, and its late end changes nothing.
#[test]
fn a_blocked_drop_after_a_failed_initialize_does_not_delay_the_failed_status() {
    run(async {
        let mut h = harness(&[("t.a", &[])]).await;
        let probe = Arc::clone(h.probe("t.a"));
        probe.script(|script| {
            script.initialize = Behavior::Fail;
            script.drop = DropBehavior::Block;
        });
        let enabling = {
            let manager = h.manager.clone();
            tokio::spawn(async move {
                manager
                    .apply(ChangeAction::Enable, id("t.a"), confirmed(&[]))
                    .await
            })
        };
        wait_until(|| kind(&h.status("t.a").state) == "Failed(Initialize)").await;
        wait_for_thread(|| count(&probe.dropped) == 1);
        assert_eq!(count(&probe.drops_finished), 0, "the drop is still blocked");
        assert!(
            !enabling.is_finished(),
            "the manager still waits for the drop"
        );
        assert_eq!(
            h.changes(),
            [
                change("t.a", true, "Stopped"),
                change("t.a", true, "Initializing"),
                change("t.a", true, "Failed(Initialize)"),
            ],
            "Failed was published while the drop was blocked"
        );

        probe.release_drops();
        let outcome = enabling.await.expect("enable task").expect("enable");
        let ApplyOutcome::Applied(report) = outcome else {
            panic!("not applied: {outcome:?}");
        };
        assert_eq!(report.failed[0].error, "fake initialize error");
        assert_eq!(count(&probe.drops_finished), 1);
        settle().await;
        assert!(h.changes().is_empty(), "the late drop changed nothing");
        assert_eq!(failed_error(&h.status("t.a")), "fake initialize error");
    });
}

/// Test 76 (D1). At the exit deadline a module is terminated: its drop starts
/// off the manager task and is not waited for, so a `Drop` that panics there
/// does not stop the exit, and its late end changes nothing.
#[test]
fn a_panicking_drop_of_a_terminated_module_does_not_stop_the_exit() {
    run(async {
        let modules: [(&str, &[&str]); 4] =
            [("t.w", &[]), ("t.x", &[]), ("t.y", &[]), ("t.z", &[])];
        let mut h = harness(&modules).await;
        for (module, _) in modules {
            h.confirm(ChangeAction::Enable, module).await;
        }
        // Exit order z, y, x, w (as in the MM-4 deadline test): z hangs until
        // its stop timeout (5 s), y stops after 4 s, x hangs and is cut off at
        // the deadline (10 s), w gets no turn, is terminated and panics in
        // Drop.
        h.probe("t.z").script(|script| script.stop = Behavior::Hang);
        h.probe("t.y")
            .script(|script| script.stop = Behavior::Delay(Duration::from_secs(4)));
        h.probe("t.x").script(|script| script.stop = Behavior::Hang);
        h.probe("t.w")
            .script(|script| script.drop = DropBehavior::Panic);
        h.changes();

        let started = Instant::now();
        let report = h.manager.shutdown().await.expect("shutdown");
        assert_eq!(started.elapsed(), SHUTDOWN_TIMEOUT);
        let errors: Vec<(String, String)> = report
            .failed
            .iter()
            .map(|failure| (failure.id.to_string(), failure.error.clone()))
            .collect();
        assert_eq!(report.stopped, [id("t.y")]);
        assert_eq!(
            errors,
            [
                ("t.z".to_owned(), text::TIMED_OUT.to_owned()),
                ("t.x".to_owned(), text::SHUTDOWN_DEADLINE.to_owned()),
                ("t.w".to_owned(), text::SHUTDOWN_DEADLINE.to_owned()),
            ]
        );
        assert_eq!(count(&h.probe("t.w").stopped), 0, "w got no stop");
        h.changes();
        settle().await;
        assert_eq!(count(&h.probe("t.w").dropped), 1, "w was dropped");
        assert_eq!(count(&h.probe("t.w").drops_finished), 0, "and panicked");
        assert!(h.changes().is_empty(), "the late drop changed nothing");
        assert_eq!(failed_error(&h.status("t.w")), text::SHUTDOWN_DEADLINE);
    });
}

/// Test 66 (D1, MM-4 regression). An `initialize` interrupted by the exit is
/// aborted; the instance is dropped by tokio inside the aborted task, so a
/// `Drop` that panics there does not affect the manager.
#[test]
fn a_panicking_drop_of_an_aborted_call_does_not_affect_the_manager() {
    run(async {
        let mut h = harness(&[("t.a", &[])]).await;
        h.probe("t.a").script(|script| {
            script.initialize = Behavior::Hang;
            script.drop = DropBehavior::Panic;
        });
        let enabling = {
            let manager = h.manager.clone();
            tokio::spawn(async move {
                manager
                    .apply(ChangeAction::Enable, id("t.a"), confirmed(&[]))
                    .await
            })
        };
        wait_until(|| h.status("t.a").state == ModuleState::Initializing).await;
        let report = h.manager.shutdown().await.expect("shutdown");
        assert_eq!(report, StopReport::default());
        let status = h.status("t.a");
        assert_eq!(kind(&status.state), "Failed(Initialize)");
        assert_eq!(failed_error(&status), text::INTERRUPTED);
        enabling.await.expect("enable task").expect("enable");
        settle().await;
        assert_eq!(count(&h.probe("t.a").dropped), 1, "dropped by tokio");
        h.changes();
    });
}

/// Test 68 (D1). Real time: a frozen clock cannot model a `Drop` that blocks
/// a pool thread. The drop of t.b blocks past the rest of its stop budget:
/// the manager stops waiting, t.b ends `Failed { Stop, TIMED_OUT }`, t.a is
/// still stopped, the reply comes while the drop is still blocked, and the
/// late end of the drop changes nothing. Only lower time bounds are checked.
#[test]
fn a_drop_that_does_not_return_in_time_fails_the_stop_and_the_exit_goes_on() {
    let logs = capture_logs(|| {
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("test runtime")
            .block_on(async {
                tokio::time::timeout(Duration::from_secs(60), async {
                    let mut h = harness(&[("t.a", &[]), ("t.b", &[])]).await;
                    h.confirm(ChangeAction::Enable, "t.a").await;
                    h.confirm(ChangeAction::Enable, "t.b").await;
                    let probe = Arc::clone(h.probe("t.b"));
                    probe.script(|script| script.drop = DropBehavior::Block);
                    h.changes();

                    let started = std::time::Instant::now();
                    let report = h.manager.shutdown().await.expect("shutdown");
                    assert!(
                        started.elapsed() >= STOP_TIMEOUT,
                        "the drop got the rest of the stop budget"
                    );
                    assert_eq!(count(&probe.dropped), 1, "the drop began");
                    assert_eq!(count(&probe.drops_finished), 0, "and is still blocked");
                    assert_eq!(report.stopped, [id("t.a")]);
                    assert_eq!(report.failed.len(), 1);
                    assert_eq!(report.failed[0].id, id("t.b"));
                    assert_eq!(report.failed[0].error, text::TIMED_OUT);
                    let mut expected = vec![
                        change("t.b", true, "Stopping"),
                        change("t.b", true, "Failed(Stop)"),
                    ];
                    expected.extend(stop_cycle("t.a", true));
                    assert_eq!(h.changes(), expected);

                    probe.release_drops();
                    wait_for_thread(|| count(&probe.drops_finished) == 1);
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    assert!(h.changes().is_empty(), "the late drop changed nothing");
                    assert_eq!(failed_error(&h.status("t.b")), text::TIMED_OUT);
                })
                .await
                .expect("the test finished within a minute");
            });
    });
    let line = log_line(&logs, "module instance drop did not finish in time");
    assert!(
        line.contains("ERROR") && line.contains("module=t.b"),
        "{line}"
    );
    let line = log_line(&logs, "module instance drops not confirmed at exit");
    assert!(line.contains("WARN") && line.contains("t.b"), "{line}");
}

/// Lets every ready task on this thread run a few times: enough for a
/// background task to end and its supervisor to send the report.
async fn run_ready_tasks() {
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
}

/// Test 50 (D2). A failure report that waits in the queue behind a command
/// while the exit is requested is only logged: no `Failed { Run }`, no
/// cascade; the module and its dependent are stopped by `shutdown_all`, and
/// the result of that stop decides their status.
#[test]
fn a_queued_failure_report_is_not_handled_after_the_exit_request() {
    let logs = capture_logs(|| {
        run(async {
            let mut h = harness(&[("t.a", &[]), ("t.b", &["t.a"]), ("t.c", &[])]).await;
            h.confirm(ChangeAction::Enable, "t.b").await;
            h.probe("t.c")
                .script(|script| script.initialize = Behavior::Hang);
            h.changes();
            let enabling = {
                let manager = h.manager.clone();
                tokio::spawn(async move {
                    manager
                        .apply(ChangeAction::Enable, id("t.c"), confirmed(&[]))
                        .await
                })
            };
            wait_until(|| h.status("t.c").state == ModuleState::Initializing).await;
            // The report of t.a waits behind the enable of t.c.
            assert!(h.probe("t.a").end_background(TaskEnd::Fail));
            wait_until(|| count(&h.probe("t.a").background) == 0).await;
            run_ready_tasks().await;

            let report = h.manager.shutdown().await.expect("shutdown");
            enabling.await.expect("enable task").expect("enable");
            assert_eq!(report.stopped, [id("t.b"), id("t.a")]);
            assert!(report.failed.is_empty());
            let mut expected = vec![
                change("t.c", true, "Stopped"),
                change("t.c", true, "Initializing"),
                change("t.c", true, "Failed(Initialize)"),
            ];
            expected.extend(stop_cycle("t.b", true));
            expected.extend(stop_cycle("t.a", true));
            assert_eq!(h.changes(), expected, "no Failed(Run), no Blocked");
            assert_eq!(
                count(&h.probe("t.a").stopped),
                1,
                "t.a was stopped with stop"
            );
        });
    });
    let line = log_line(&logs, "background task failed after the exit request");
    assert!(line.contains("WARN"), "{line}");
    assert!(line.contains("module=t.a"), "{line}");
    assert!(
        line.contains("fake background error"),
        "the text is kept: {line}"
    );
    assert!(
        !logs
            .lines()
            .any(|line| line.contains("ERROR") && line.contains("background task failed")),
        "the report was not handled as a failure of a running module:\n{logs}"
    );
}

/// Test 53 (D2). The report of an instance stopped before the exit, read
/// after the exit request, is a `debug` line with the text.
#[test]
fn a_report_of_a_stopped_instance_is_logged_by_its_level() {
    let logs = capture_logs(|| {
        run(async {
            let mut h = harness(&[("t.a", &[])]).await;
            h.probe("t.a")
                .script(|script| script.background = Background::FailsOnCancel);
            h.confirm(ChangeAction::Enable, "t.a").await;
            // The restart stops the old instance (its task fails while it
            // stops, the report is queued) and then hangs in the new
            // instance's initialize until the exit interrupts it.
            h.probe("t.a").script(|script| {
                script.background = Background::Cooperative;
                script.initialize = Behavior::Hang;
            });
            h.changes();
            let restarting = {
                let manager = h.manager.clone();
                tokio::spawn(async move {
                    manager
                        .apply(ChangeAction::Restart, id("t.a"), confirmed(&[]))
                        .await
                })
            };
            // The restart drops the old instance on the blocking pool first.
            wait_until_with_pool(|| h.status("t.a").state == ModuleState::Initializing).await;
            run_ready_tasks().await;
            let report = h.manager.shutdown().await.expect("shutdown");
            restarting.await.expect("restart task").expect("restart");
            assert_eq!(report, StopReport::default());
            let status = h.status("t.a");
            assert_eq!(kind(&status.state), "Failed(Initialize)");
            assert_eq!(failed_error(&status), text::INTERRUPTED);
        });
    });
    let line = log_line(
        &logs,
        "background task of a stopped instance failed after the exit request",
    );
    assert!(line.contains("DEBUG"), "{line}");
    assert!(line.contains("module=t.a"), "{line}");
    assert!(
        line.contains("fake error while stopping"),
        "the text is kept: {line}"
    );
    assert!(
        !logs.contains("background task failed after the exit request"),
        "{logs}"
    );
}

/// Test 74 (D2). A report the manager can no longer take — its queue is
/// closed, or the manager is gone — is logged by the supervisor itself.
#[test]
fn a_failure_that_cannot_be_reported_is_logged() {
    let logs = capture_logs(|| {
        run(async {
            // The queue is closed: `send` fails.
            let (reports, queue) = tokio::sync::mpsc::unbounded_channel::<manager::Command>();
            drop(queue);
            let context = ModuleContext::new(
                id("t.a"),
                7,
                Arc::default(),
                EventBus::new(16),
                reports.downgrade(),
            );
            context.spawn("closed queue", async {
                Err(ModuleFailure::new("fake error after the exit"))
            });
            wait_until(|| context.tasks_alive() == 0).await;
            drop(reports);

            // No sender is left: `upgrade` fails.
            let (reports, _queue) = tokio::sync::mpsc::unbounded_channel::<manager::Command>();
            let weak = reports.downgrade();
            drop(reports);
            let context = ModuleContext::new(id("t.b"), 8, Arc::default(), EventBus::new(16), weak);
            context.spawn("no manager", async {
                panic!("fake panic after the exit in /secret/path/late.rs")
            });
            wait_until(|| context.tasks_alive() == 0).await;
        });
    });
    let lines: Vec<&str> = logs
        .lines()
        .filter(|line| line.contains("the module manager can no longer be told"))
        .collect();
    assert_eq!(lines.len(), 2, "{logs}");
    assert!(
        lines[0].contains("WARN") && lines[1].contains("WARN"),
        "{logs}"
    );
    assert!(
        lines[0].contains("module=t.a")
            && lines[0].contains("instance=7")
            && lines[0].contains("closed queue")
            && lines[0].contains("fake error after the exit"),
        "{}",
        lines[0]
    );
    assert!(
        lines[1].contains("module=t.b") && lines[1].contains("fake panic after the exit"),
        "{}",
        lines[1]
    );
}

/// Test 71 (D2). Messages that arrive while `shutdown_all` runs start
/// nothing: a failure report is only logged and its module is still stopped
/// with `stop` (`Stopped`, its dependent not `Blocked`); commands get
/// `ShutDown`; the end of a late health check is ignored; the only events are
/// those of `shutdown_all`.
#[test]
fn messages_arriving_during_shutdown_all_start_nothing() {
    let logs = capture_logs(|| {
        run(async {
            let mut h = harness(&[("t.a", &[]), ("t.b", &["t.a"]), ("t.h", &[])]).await;
            h.confirm(ChangeAction::Enable, "t.b").await;
            h.confirm(ChangeAction::Enable, "t.h").await;
            // t.h: a health check that answers after 7 s, so it still holds
            // the instance when the exit begins and ends during the stops.
            h.probe("t.h").script(|script| {
                script.health = HealthBehavior::Late {
                    after: Duration::from_secs(7),
                    panic: false,
                };
            });
            assert_eq!(
                h.manager.check_health(id("t.h")).await,
                Ok(Health::Unhealthy(text::HEALTH_NO_ANSWER.to_owned()))
            );
            h.probe("t.b")
                .script(|script| script.stop = Behavior::Delay(Duration::from_secs(3)));
            h.changes();

            let exiting = {
                let manager = h.manager.clone();
                tokio::spawn(async move { manager.shutdown().await })
            };
            tokio::time::sleep(Duration::from_secs(1)).await;
            assert!(h.probe("t.a").end_background(TaskEnd::Fail));
            let disabling = {
                let manager = h.manager.clone();
                tokio::spawn(async move {
                    manager
                        .apply(ChangeAction::Disable, id("t.a"), confirmed(&["t.b"]))
                        .await
                })
            };
            let checking = {
                let manager = h.manager.clone();
                tokio::spawn(async move { manager.check_health(id("t.a")).await })
            };

            let report = exiting.await.expect("exit task").expect("shutdown");
            assert_eq!(disabling.await.expect("task"), Err(ManagerError::ShutDown));
            assert_eq!(checking.await.expect("task"), Err(ManagerError::ShutDown));
            assert!(report.failed.is_empty(), "{report:?}");
            assert_eq!(report.stopped.len(), 3);
            for module in ["t.a", "t.b", "t.h"] {
                assert_eq!(
                    h.status(module),
                    ModuleStatus {
                        enabled: Some(true),
                        state: ModuleState::Stopped
                    },
                    "{module}"
                );
                assert_eq!(count(&h.probe(module).stopped), 1, "{module}: one stop");
            }
            let changes = h.changes();
            assert_eq!(changes.len(), 6, "{changes:?}");
            assert!(
                changes
                    .iter()
                    .all(|(_, enabled, state)| *enabled == Some(true)
                        && (state == "Stopping" || state == "Stopped")),
                "only the stops of shutdown_all: {changes:?}"
            );
            settle().await;
            assert!(h.events().is_empty(), "nothing after the reply");
        });
    });
    let line = log_line(&logs, "background task failed after the exit request");
    assert!(
        line.contains("WARN") && line.contains("module=t.a"),
        "{line}"
    );
    assert!(line.contains("fake background error"), "{line}");
    log_line(&logs, "stopping modules: the application is exiting");
    assert!(!logs.contains("Failed { phase: Run"), "{logs}");
}

/// A waker that logs `marker` when woken. A request polled with it shows in
/// the manager's log the moment the manager answers or releases it: sending
/// on or dropping a `oneshot::Sender` wakes the receiver at once, on the
/// manager's thread.
struct LogOnWake(&'static str);

impl std::task::Wake for LogOnWake {
    fn wake(self: Arc<Self>) {
        tracing::info!(target: "test", "{}", self.0);
    }
}

/// Test 72 (D2, L1). A repeated exit request is not run again, and the
/// manager releases it after it has logged its end and before it answers the
/// first request: end line → release → reply, as the log of the manager's
/// thread shows (both requests are polled with marking wakers).
#[test]
fn a_repeated_shutdown_is_released_after_the_end_is_logged() {
    let logs = capture_logs(|| {
        run(async {
            let h = harness(&[("t.a", &[])]).await;
            h.confirm(ChangeAction::Enable, "t.a").await;
            h.probe("t.a")
                .script(|script| script.stop = Behavior::Delay(Duration::from_secs(2)));
            let answered = Waker::from(Arc::new(LogOnWake("FIRST EXIT REQUEST ANSWERED")));
            let released = Waker::from(Arc::new(LogOnWake("REPEATED EXIT REQUEST RELEASED")));
            let manager = h.manager.clone();
            let mut first = Box::pin(async move { manager.shutdown().await });
            assert!(
                first
                    .as_mut()
                    .poll(&mut Context::from_waker(&answered))
                    .is_pending()
            );
            wait_until(|| h.status("t.a").state == ModuleState::Stopping).await;
            // The repeated request is queued while the modules stop.
            let manager = h.manager.clone();
            let mut second = Box::pin(async move { manager.shutdown().await });
            assert!(
                second
                    .as_mut()
                    .poll(&mut Context::from_waker(&released))
                    .is_pending()
            );
            settle().await;
            let Poll::Ready(Ok(report)) = first.as_mut().poll(&mut Context::from_waker(&answered))
            else {
                panic!("the first request is answered");
            };
            assert_eq!(report.stopped, [id("t.a")]);
            assert_eq!(
                second.as_mut().poll(&mut Context::from_waker(&released)),
                Poll::Ready(Err(ManagerError::ShutDown))
            );
            assert_eq!(count(&h.probe("t.a").stopped), 1, "shutdown_all ran once");
        });
    });
    log_line(&logs, "stopping modules: the application is exiting");
    let position = |message: &str| {
        logs.find(message)
            .unwrap_or_else(|| panic!("no `{message}` in the log:\n{logs}"))
    };
    let end = position("module manager stopped");
    let released = position("REPEATED EXIT REQUEST RELEASED");
    let answered = position("FIRST EXIT REQUEST ANSWERED");
    assert!(end < released, "released only after the end line:\n{logs}");
    assert!(released < answered, "released before the reply:\n{logs}");
}

/// Test 78 (variant B). A request that finds the queue already closed fails
/// at once, so the queue is closed only after the end line. With more than
/// tokio's cooperative budget (128) of messages queued, a drain with `recv`
/// after `close` yields in the middle; a request sent then would fail before
/// the end line. Here such a request, sent as soon as the modules have
/// stopped, fails only after the end line.
#[test]
fn a_request_that_finds_the_queue_closed_fails_after_the_end_is_logged() {
    let logs = capture_logs(|| {
        run(async {
            let h = harness(&[("t.a", &[])]).await;
            h.confirm(ChangeAction::Enable, "t.a").await;
            h.probe("t.a")
                .script(|script| script.stop = Behavior::Delay(Duration::from_secs(2)));
            let first = {
                let manager = h.manager.clone();
                tokio::spawn(async move { manager.shutdown().await })
            };
            wait_until(|| h.status("t.a").state == ModuleState::Stopping).await;
            // 200 commands wait in the queue behind the exit.
            let waiting: Vec<_> = (0..200)
                .map(|_| {
                    let manager = h.manager.clone();
                    tokio::spawn(async move { manager.check_health(id("t.a")).await })
                })
                .collect();
            run_ready_tasks().await;
            // A late request, woken by the last status change of the exit.
            let late = {
                let manager = h.manager.clone();
                let mut events = h.bus.subscribe();
                tokio::spawn(async move {
                    loop {
                        let Received::Event(envelope) = events.recv().await else {
                            panic!("bus closed");
                        };
                        let AppEvent::Module(ModuleEvent::StatusChanged { current, .. }) =
                            envelope.event
                        else {
                            continue;
                        };
                        if current.state == ModuleState::Stopped {
                            break;
                        }
                    }
                    let result = manager.shutdown().await;
                    tracing::info!(target: "test", "LATE EXIT REQUEST RETURNED");
                    result
                })
            };
            first.await.expect("task").expect("first shutdown");
            assert_eq!(late.await.expect("task"), Err(ManagerError::ShutDown));
            for task in waiting {
                assert_eq!(task.await.expect("task"), Err(ManagerError::ShutDown));
            }
        });
    });
    let end = logs.find("module manager stopped").expect("end logged");
    let late = logs
        .find("LATE EXIT REQUEST RETURNED")
        .expect("late request returned");
    assert!(end < late, "{logs}");
}

/// Test 57 (L1). The manager logs its end before it replies to `Shutdown`,
/// and after the reply it neither logs nor publishes anything. A hook right
/// after the reply marks the moment in the log of the manager's thread, so
/// the order itself is checked, not only the presence of the line.
#[test]
fn the_manager_logs_its_end_before_replying() {
    let logs = capture_logs(|| {
        run(async {
            let mut h = harness(&[("t.a", &[]), ("t.b", &["t.a"])]).await;
            h.confirm(ChangeAction::Enable, "t.b").await;
            h.probe("t.b")
                .script(|script| script.stop = Behavior::Delay(Duration::from_secs(2)));
            h.changes();
            manager::test_hooks::AFTER_REPLY.set(Some(Box::new(|| {
                tracing::info!(target: "test", "AFTER THE REPLY");
            })));
            let exiting = {
                let manager = h.manager.clone();
                tokio::spawn(async move { manager.shutdown().await })
            };
            tokio::time::sleep(Duration::from_secs(1)).await;
            // A report that the manager takes from its queue after the stops.
            assert!(h.probe("t.a").end_background(TaskEnd::Fail));
            let report = exiting.await.expect("task").expect("shutdown");
            assert_eq!(report.stopped, [id("t.b"), id("t.a")]);
            let mut expected = stop_cycle("t.b", true);
            expected.extend(stop_cycle("t.a", true));
            assert_eq!(h.changes(), expected);
            settle().await;
            assert!(h.events().is_empty(), "nothing published after the reply");
            manager::test_hooks::AFTER_REPLY.set(None);
        });
    });
    let position = |message: &str| {
        logs.find(message)
            .unwrap_or_else(|| panic!("no `{message}` in the log:\n{logs}"))
    };
    let report = position("background task failed after the exit request");
    let stopped = position("module manager stopped");
    let marker = position("AFTER THE REPLY");
    assert!(
        report < stopped,
        "the queue is taken before the end:\n{logs}"
    );
    assert!(
        stopped < marker,
        "the end is logged before the reply:\n{logs}"
    );
    let after = &logs[marker..];
    assert!(
        !after.contains("atlas_lib::modules"),
        "the manager logs nothing after the reply:\n{after}"
    );
}
