//! Module lifecycle manager (issue #9, ADR-014).
//!
//! An actor: one task owns the registry and handles commands strictly one
//! after another. It is the only component that changes module statuses and
//! it keeps the status invariant on every transition (decision 23 in #13).
//! Mechanisms (decision 26): `mpsc` carries commands to the actor, `oneshot`
//! carries each result back, `watch` holds a snapshot of the statuses for
//! reading only, and every change is announced only by `StatusChanged` on the
//! central event bus.
//!
//! Nothing happens on its own: modules start, stop and recover only by a
//! confirmed command. Failures are isolated: every module call runs in its own
//! task with a timeout, and a failed module blocks only its dependents.

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use thiserror::Error;
use tokio::sync::mpsc::{self, WeakUnboundedSender};
use tokio::sync::{oneshot, watch};
use tokio::task::JoinHandle;
use tokio::time::{Instant, timeout};
use tokio_util::sync::CancellationToken;

use super::context::{ModuleContext, TaskFailure, panic_text};
use super::contract::{
    BlockReason, FailurePhase, Health, Module, ModuleError, ModuleFactory, ModuleFailure, ModuleId,
    ModuleState, ModuleStatus,
};
use super::graph::ModuleGraph;
use super::plan::{
    self, ChangeAction, ChangePlan, Confirmation, PlanStep, StatusMap, Verification,
};
use crate::clock::unix_time_ms;
use crate::events::bus::EventBus;
use crate::events::types::{AppEvent, ModuleEvent};

/// Limit for `initialize`. Generous because a module may check local engines
/// or connections; heavy loading belongs in background tasks.
pub const INITIALIZE_TIMEOUT: Duration = Duration::from_secs(30);
/// Limit for `start`, which only launches the work.
pub const START_TIMEOUT: Duration = Duration::from_secs(10);
/// Limit for `stop`: a user who disables a module expects it to stop quickly.
pub const STOP_TIMEOUT: Duration = Duration::from_secs(5);
/// Time the background tasks of a stopping instance get after the
/// cancellation before they are aborted.
pub const TASKS_STOP_TIMEOUT: Duration = Duration::from_secs(2);
/// Limit for an on-demand health check.
pub const HEALTH_CHECK_TIMEOUT: Duration = Duration::from_secs(5);
/// Overall limit for stopping all modules when the application exits.
pub const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(10);

/// Texts shown to the user in `Failed { error }` and health results. Panic
/// messages, paths and other details go to the log only.
pub mod text {
    pub const PANICKED: &str = "Модуль аварийно завершился";
    pub const TIMED_OUT: &str = "Модуль не ответил вовремя";
    pub const INTERRUPTED: &str = "Операция прервана завершением приложения";
    pub const TASKS_ABORTED: &str = "Фоновые задачи модуля не завершились вовремя и прерваны";
    pub const SHUTDOWN_DEADLINE: &str = "Модуль не успел остановиться при завершении приложения";
    pub const UNRESPONSIVE: &str = "Модуль не отвечает и остановлен без вызова stop";
    pub const HEALTH_NO_ANSWER: &str = "проверка не ответила";
    pub const HEALTH_PANICKED: &str = "проверка завершилась аварийно";
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ManagerError {
    #[error(transparent)]
    Module(#[from] ModuleError),
    #[error("module `{0}` is not running")]
    NotRunning(ModuleId),
    /// The application is exiting: the manager accepts no more commands.
    #[error("the module manager has shut down")]
    ShutDown,
}

/// Result of `apply`. Only `Applied` changed anything.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApplyOutcome {
    Applied(ApplyReport),
    NotApplicable(ChangePlan),
    ConfirmationRequired(ChangePlan),
    ImpactChanged(ChangePlan),
}

/// A module that failed, with the text shown to the user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailedModule {
    pub id: ModuleId,
    pub error: String,
}

/// What an applied plan did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ApplyReport {
    /// Started by the plan, now `Running`.
    pub started: Vec<ModuleId>,
    /// Stopped by the plan.
    pub stopped: Vec<ModuleId>,
    /// Failed while the plan ran, now `Failed`. Not retried.
    pub failed: Vec<FailedModule>,
    /// Not started because a dependency failed while the plan ran, now
    /// `Blocked { DependencyFailed }`.
    pub blocked: Vec<ModuleId>,
    /// The application started to exit; the remaining steps were skipped.
    pub interrupted: bool,
}

/// Result of stopping modules at exit; every failure is also logged.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StopReport {
    pub stopped: Vec<ModuleId>,
    pub failed: Vec<FailedModule>,
}

/// Exit signals shared by the handle and the actor: the actor may be busy
/// with a command when the application starts to exit.
#[derive(Clone, Default)]
struct ShutdownSignal {
    /// Exit requested: nothing new starts; `initialize`, `start` and health
    /// checks in progress are interrupted.
    requested: CancellationToken,
    /// `SHUTDOWN_TIMEOUT` after the request: stops still in progress are given
    /// up and the modules left are terminated.
    expired: CancellationToken,
}

/// Clonable handle to the manager, held by `AppState`.
#[derive(Clone)]
pub struct ModuleManagerHandle {
    commands: mpsc::UnboundedSender<Command>,
    snapshot: watch::Receiver<StatusMap>,
    graph: Arc<ModuleGraph>,
    shutdown: ShutdownSignal,
}

impl ModuleManagerHandle {
    /// Current statuses of all modules, read from the snapshot without
    /// waiting for the manager. Read-only: changes are announced only by
    /// `StatusChanged` on the event bus (decision 26).
    pub fn statuses(&self) -> StatusMap {
        self.snapshot.borrow().clone()
    }

    /// The plan to show the user before confirmation, computed from the
    /// snapshot. `apply` computes it again from the manager's own statuses.
    pub fn plan(&self, action: ChangeAction, target: &ModuleId) -> Result<ChangePlan, ModuleError> {
        plan::plan(&self.graph, &self.statuses(), action, target)
    }

    /// Applies `action` to `target` if `verify` approves `confirmation` for
    /// the plan as it is now; otherwise nothing changes.
    pub async fn apply(
        &self,
        action: ChangeAction,
        target: ModuleId,
        confirmation: Confirmation,
    ) -> Result<ApplyOutcome, ManagerError> {
        self.request(|reply| Command::Apply {
            action,
            target,
            confirmation,
            reply,
        })
        .await?
    }

    /// Runs the health check of a running module. The result is informational
    /// and never changes the status, except when the check panics: the
    /// instance is then lost and the module fails.
    pub async fn check_health(&self, id: ModuleId) -> Result<Health, ManagerError> {
        self.request(|reply| Command::CheckHealth { id, reply })
            .await?
    }

    /// Stops all running modules for the application exit, in reverse
    /// dependency order, within `SHUTDOWN_TIMEOUT` counted from this call
    /// (decision 28). Modules left when the time is up are terminated. The
    /// manager then logs its end and only then replies; after the reply it
    /// logs, publishes and does nothing more, and later commands fail with
    /// `ShutDown`, so the event bus can be closed safely. Must be called
    /// within the tokio runtime: it starts the deadline timer.
    pub async fn shutdown(&self) -> Result<StopReport, ManagerError> {
        if !self.shutdown.requested.is_cancelled() {
            self.shutdown.requested.cancel();
            let expired = self.shutdown.expired.clone();
            tokio::spawn(async move {
                tokio::time::sleep(SHUTDOWN_TIMEOUT).await;
                expired.cancel();
            });
        }
        self.request(|reply| Command::Shutdown { reply }).await
    }

    async fn request<T>(
        &self,
        command: impl FnOnce(oneshot::Sender<T>) -> Command,
    ) -> Result<T, ManagerError> {
        let (reply, answer) = oneshot::channel();
        self.commands
            .send(command(reply))
            .map_err(|_| ManagerError::ShutDown)?;
        answer.await.map_err(|_| ManagerError::ShutDown)
    }
}

/// Creates the manager for `catalog`. Returns the handle and the actor; the
/// caller runs the actor on its runtime (`tauri::async_runtime::spawn` in the
/// application, a test runtime in tests). No module starts here: all are
/// disabled and stopped (autostart is #10).
///
/// An invalid catalog is a programming error caught by tests; if it happens
/// anyway, it is logged and the application runs without modules.
pub fn create(
    catalog: Vec<Box<dyn ModuleFactory>>,
    events: EventBus,
) -> (
    ModuleManagerHandle,
    impl Future<Output = ()> + Send + 'static,
) {
    let (graph, factories) = match ModuleGraph::build(
        catalog.iter().map(|factory| factory.descriptor().clone()),
    ) {
        Ok(graph) => {
            let factories: BTreeMap<ModuleId, Box<dyn ModuleFactory>> = catalog
                .into_iter()
                .map(|factory| (factory.descriptor().id.clone(), factory))
                .collect();
            (graph, factories)
        }
        Err(error) => {
            tracing::error!(%error, "invalid module catalog: the application runs without modules");
            (ModuleGraph::default(), BTreeMap::new())
        }
    };
    let graph = Arc::new(graph);
    let statuses: StatusMap = graph
        .start_order()
        .iter()
        .map(|id| (id.clone(), ModuleStatus::default()))
        .collect();
    let (snapshot_tx, snapshot_rx) = watch::channel(statuses.clone());
    let (commands_tx, commands_rx) = mpsc::unbounded_channel();
    let shutdown = ShutdownSignal::default();
    let actor = Actor {
        graph: Arc::clone(&graph),
        factories,
        statuses,
        running: BTreeMap::new(),
        health: BTreeMap::new(),
        snapshot: snapshot_tx,
        events,
        reports: commands_tx.downgrade(),
        shutdown: shutdown.clone(),
        next_instance: 0,
        pending_drops: Vec::new(),
    };
    let handle = ModuleManagerHandle {
        commands: commands_tx,
        snapshot: snapshot_rx,
        graph,
        shutdown,
    };
    (handle, actor.run(commands_rx))
}

/// Commands to the actor (`mpsc`); each result goes back by `oneshot`.
pub(super) enum Command {
    Apply {
        action: ChangeAction,
        target: ModuleId,
        confirmation: Confirmation,
        reply: oneshot::Sender<Result<ApplyOutcome, ManagerError>>,
    },
    CheckHealth {
        id: ModuleId,
        reply: oneshot::Sender<Result<Health, ManagerError>>,
    },
    Shutdown {
        reply: oneshot::Sender<StopReport>,
    },
    /// From the supervisor of a background task (`ModuleContext::spawn`).
    TaskFailed {
        id: ModuleId,
        instance: u64,
        task: &'static str,
        failure: TaskFailure,
    },
    /// A health check task has ended (also by a panic or an abort), so an
    /// instance still held by a late check can be taken back at once.
    HealthCheckEnded {
        id: ModuleId,
        instance: u64,
    },
}

/// A started instance of a running module.
struct Instance {
    context: ModuleContext,
    slot: Slot,
}

enum Slot {
    Ready(Box<dyn Module>),
    /// A health check that did not answer in time still holds the instance.
    Checking(JoinHandle<(Box<dyn Module>, Health)>),
    /// The instance panicked inside a call and is gone.
    Lost,
}

struct Actor {
    graph: Arc<ModuleGraph>,
    factories: BTreeMap<ModuleId, Box<dyn ModuleFactory>>,
    /// Authoritative statuses; the snapshot is a copy for readers.
    statuses: StatusMap,
    /// Instances of the modules in `Running`, and only of those.
    running: BTreeMap<ModuleId, Instance>,
    /// Last health result of each running instance.
    health: BTreeMap<ModuleId, Health>,
    snapshot: watch::Sender<StatusMap>,
    events: EventBus,
    /// Given to module contexts for failure reports. Weak: modules do not
    /// keep the manager alive.
    reports: WeakUnboundedSender<Command>,
    shutdown: ShutdownSignal,
    next_instance: u64,
    /// Drops the manager stopped waiting for. They may still run on the
    /// blocking pool; their results are never read, so a late result changes
    /// nothing.
    pending_drops: Vec<PendingDrop>,
}

/// A drop of a module instance that was not confirmed in time.
struct PendingDrop {
    module: ModuleId,
    task: JoinHandle<()>,
}

/// A lifecycle call that did not succeed: the text for the user and, when the
/// module returned an error, the instance, which the caller drops off the
/// manager task (D1).
struct CallFailure {
    error: String,
    instance: Option<Box<dyn Module>>,
}

impl Actor {
    async fn run(mut self, mut commands: mpsc::UnboundedReceiver<Command>) {
        tracing::info!(modules = self.statuses.len(), "module manager started");
        while let Some(command) = commands.recv().await {
            match command {
                Command::Apply {
                    action,
                    target,
                    confirmation,
                    reply,
                } => {
                    let outcome = self.apply(action, &target, &confirmation).await;
                    let _ = reply.send(outcome);
                }
                Command::CheckHealth { id, reply } => {
                    let health = self.check_health(&id).await;
                    let _ = reply.send(health);
                }
                Command::TaskFailed {
                    id,
                    instance,
                    task,
                    failure,
                } => self.task_failed(&id, instance, task, failure).await,
                Command::HealthCheckEnded { id, instance } => {
                    self.health_check_ended(&id, instance).await;
                }
                Command::Shutdown { reply } => {
                    self.exit(reply, &mut commands).await;
                    // Nothing after the reply: no log, no event, no work (L1).
                    return;
                }
            }
        }
        // Every handle is gone and no exit was requested: there is nobody to
        // reply to. Nothing is published after this point.
        if !self.running.is_empty() {
            tracing::warn!(
                modules = self.running.len(),
                "module manager ended with modules still running"
            );
        }
        // No instance is dropped in the manager task (D1): the instances left
        // are dropped on the blocking pool, nobody waits for them.
        for (_, Instance { slot, .. }) in std::mem::take(&mut self.running) {
            if let Slot::Ready(module) = slot {
                drop(drop_off_task(module));
            }
        }
        tracing::info!("module manager stopped");
    }

    /// The exit command (decision 28, D2, L1): stop the modules, take what is
    /// left in the queue (reports are only logged, commands get `ShutDown`),
    /// log the end of the manager, and only then send the report. After the
    /// reply the manager does nothing more.
    ///
    /// Order (variant B): the queue is closed only after the end is logged.
    /// A request that finds the queue closed — e.g. a second exit request from
    /// `RunEvent::Exit` — fails at once, so it must not be able to fail before
    /// the end line, or its caller could flush the log before that line.
    async fn exit(
        &mut self,
        reply: oneshot::Sender<StopReport>,
        commands: &mut mpsc::UnboundedReceiver<Command>,
    ) {
        // Instances running when the exit began, to choose the log level of
        // their late reports.
        let running_at_exit: BTreeSet<(ModuleId, u64)> = self
            .running
            .iter()
            .map(|(id, instance)| (id.clone(), instance.context.instance()))
            .collect();
        let report = self.shutdown().await;

        // 1. Take what is queued while the queue is still open: requests sent
        //    meanwhile are queued too and wait for the steps below.
        let mut repeated = Vec::new();
        while let Ok(command) = commands.try_recv() {
            exit_message(command, &running_at_exit, &mut repeated);
        }
        // 2. Log the end.
        if !self.running.is_empty() {
            tracing::warn!(
                modules = self.running.len(),
                "module manager ended with modules still running"
            );
        }
        tracing::info!("module manager stopped");
        // 3. Close the queue: from now on a supervisor whose report cannot be
        //    sent logs it itself, and a new request fails at once — after the
        //    end line. 4. Take what arrived between step 1 and the close; a
        //    report found here is logged after the end line, still before the
        //    reply (the remaining limitation of this order).
        commands.close();
        while let Some(command) = commands.recv().await {
            exit_message(command, &running_at_exit, &mut repeated);
        }
        // 5. Release the repeated exit requests: their callers get `ShutDown`
        //    after the end line. 6. Reply; nothing follows.
        drop(repeated);
        let _ = reply.send(report);
        #[cfg(test)]
        test_hooks::after_reply();
    }

    async fn apply(
        &mut self,
        action: ChangeAction,
        target: &ModuleId,
        confirmation: &Confirmation,
    ) -> Result<ApplyOutcome, ManagerError> {
        if self.shutdown.requested.is_cancelled() {
            return Err(ManagerError::ShutDown);
        }
        let plan = plan::plan(&self.graph, &self.statuses, action, target)?;
        Ok(match plan::verify(plan.clone(), confirmation) {
            Verification::Approved => ApplyOutcome::Applied(self.execute(&plan).await),
            Verification::NotApplicable(plan) => ApplyOutcome::NotApplicable(plan),
            Verification::ConfirmationRequired(plan) => ApplyOutcome::ConfirmationRequired(plan),
            Verification::ImpactChanged(plan) => ApplyOutcome::ImpactChanged(plan),
        })
    }

    async fn execute(&mut self, plan: &ChangePlan) -> ApplyReport {
        tracing::info!(
            action = %plan.action,
            target = %plan.target,
            steps = plan.steps.len(),
            "applying module plan"
        );
        let mut report = ApplyReport::default();
        // Modules that failed while this plan ran: they are not started again
        // and nothing that depends on them is started (no retries).
        let mut failed = BTreeSet::new();
        for step in &plan.steps {
            if self.shutdown.requested.is_cancelled() {
                tracing::warn!(
                    action = %plan.action,
                    target = %plan.target,
                    "plan interrupted: the application is exiting"
                );
                report.interrupted = true;
                break;
            }
            match step {
                PlanStep::Enable(id) => self.set_enabled(id, true),
                PlanStep::Disable(id) => {
                    // A module whose stop just failed keeps `Failed { phase: Stop }`.
                    let state = if failed.contains(id) {
                        self.state(id)
                    } else {
                        ModuleState::Stopped
                    };
                    self.set_status(
                        id,
                        ModuleStatus {
                            enabled: false,
                            state,
                        },
                    );
                }
                PlanStep::Stop(id) => match self.stop_running(id, ModuleState::Stopped).await {
                    Ok(()) => report.stopped.push(id.clone()),
                    Err(error) => {
                        failed.insert(id.clone());
                        report.failed.push(FailedModule {
                            id: id.clone(),
                            error,
                        });
                    }
                },
                PlanStep::Start(id) => {
                    if failed.contains(id) {
                        continue;
                    }
                    if let Some(root) = self.failed_dependency(id, &failed) {
                        let by = ModuleState::Blocked {
                            by: root,
                            reason: BlockReason::DependencyFailed,
                        };
                        self.set_state(id, by);
                        report.blocked.push(id.clone());
                        continue;
                    }
                    match self.start(id).await {
                        Ok(()) => report.started.push(id.clone()),
                        Err(error) => {
                            failed.insert(id.clone());
                            report.failed.push(FailedModule {
                                id: id.clone(),
                                error,
                            });
                        }
                    }
                }
            }
        }
        report
    }

    /// The first module, in start order, among the transitive dependencies of
    /// `id` that failed while the current plan ran: the root cause.
    fn failed_dependency(&self, id: &ModuleId, failed: &BTreeSet<ModuleId>) -> Option<ModuleId> {
        let dependencies = self.graph.transitive_dependencies(id);
        self.graph
            .start_order()
            .iter()
            .find(|module| failed.contains(*module) && dependencies.contains(*module))
            .cloned()
    }

    /// Creates a new instance and runs `initialize` and `start`. On failure
    /// the module ends up `Failed` with the phase, and whatever the instance
    /// spawned is cancelled and aborted.
    async fn start(&mut self, id: &ModuleId) -> Result<(), String> {
        let Some(factory) = self.factories.get(id) else {
            // Unreachable: the graph is built from the factories.
            tracing::error!(module = %id, "no factory for module");
            let error = text::PANICKED.to_owned();
            self.set_state(id, failed(FailurePhase::Initialize, &error));
            return Err(error);
        };
        let module = factory.create();
        self.next_instance += 1;
        let context = ModuleContext::new(
            id.clone(),
            self.next_instance,
            self.events.clone(),
            self.reports.clone(),
        );

        self.set_state(id, ModuleState::Initializing);
        let module = match self.call(id, module, &context, Lifecycle::Initialize).await {
            Ok(module) => module,
            Err(failure) => {
                return Err(self
                    .fail_start(id, FailurePhase::Initialize, failure, &context)
                    .await);
            }
        };
        self.set_state(id, ModuleState::Starting);
        let module = match self.call(id, module, &context, Lifecycle::Start).await {
            Ok(module) => module,
            Err(failure) => {
                return Err(self
                    .fail_start(id, FailurePhase::Start, failure, &context)
                    .await);
            }
        };
        self.running.insert(
            id.clone(),
            Instance {
                context,
                slot: Slot::Ready(module),
            },
        );
        self.set_state(id, ModuleState::Running);
        Ok(())
    }

    /// `initialize` or `start` failed. Decision 7: `Failed { phase }` first,
    /// then the token is cancelled, the instance is dropped (off the manager
    /// task, within `STOP_TIMEOUT`) and the background tasks are waited for
    /// and aborted. A drop that blocks never delays the status. `stop` is not
    /// called because the instance never started. After a panic, a timeout or
    /// an interruption the manager has no instance to drop.
    async fn fail_start(
        &mut self,
        id: &ModuleId,
        phase: FailurePhase,
        failure: CallFailure,
        context: &ModuleContext,
    ) -> String {
        let CallFailure { error, instance } = failure;
        self.set_state(id, failed(phase, &error));
        context.cancel();
        if let Some(module) = instance {
            // The status keeps the module's own error; the drop goes to the log.
            let _ = self
                .wait_drop(id, drop_off_task(module), STOP_TIMEOUT)
                .await;
        }
        let aborted = context
            .finish_tasks(TASKS_STOP_TIMEOUT, &self.shutdown.expired)
            .await;
        if aborted > 0 {
            tracing::warn!(module = %id, aborted, "background tasks of a failed instance were aborted");
        }
        error
    }

    /// Runs a lifecycle call in its own task: a panic stays in that task, and
    /// the manager goes on if the call never returns. Returns the instance, or
    /// the text for the user with the instance if the module returned an
    /// error; details are logged.
    async fn call(
        &self,
        id: &ModuleId,
        module: Box<dyn Module>,
        context: &ModuleContext,
        call: Lifecycle,
    ) -> Result<Box<dyn Module>, CallFailure> {
        let (limit, stop_waiting) = match call {
            Lifecycle::Initialize => (INITIALIZE_TIMEOUT, &self.shutdown.requested),
            Lifecycle::Start => (START_TIMEOUT, &self.shutdown.requested),
            Lifecycle::Stop => (STOP_TIMEOUT, &self.shutdown.expired),
        };
        let task = spawn_call(module, context.clone(), call);
        match wait_for(task, limit, stop_waiting).await {
            Waited::Done((module, Ok(()))) => Ok(module),
            Waited::Done((module, Err(failure))) => {
                tracing::error!(module = %id, call = call.name(), error = %failure, "module call failed");
                Err(CallFailure {
                    error: failure.message().to_owned(),
                    instance: Some(module),
                })
            }
            Waited::Panicked(payload) => {
                tracing::error!(module = %id, call = call.name(), panic = %payload, "module call panicked");
                Err(CallFailure::without_instance(text::PANICKED))
            }
            Waited::TimedOut(task) => {
                task.abort();
                tracing::warn!(module = %id, call = call.name(), ?limit, "module call timed out");
                Err(CallFailure::without_instance(text::TIMED_OUT))
            }
            Waited::Interrupted(task) => {
                task.abort();
                tracing::warn!(module = %id, call = call.name(), "module call interrupted: the application is exiting");
                Err(CallFailure::without_instance(match call {
                    Lifecycle::Stop => text::SHUTDOWN_DEADLINE,
                    Lifecycle::Initialize | Lifecycle::Start => text::INTERRUPTED,
                }))
            }
        }
    }

    /// Waits for the drop of an instance (`drop_off_task`) at most `limit`
    /// and not after the exit deadline. Only a drop that returned counts as
    /// done. Otherwise the text for the user is returned and, if the drop may
    /// still run, it is kept in `pending_drops` and never awaited again: the
    /// waiting stops, the blocking task does not (it cannot be aborted).
    async fn wait_drop(
        &mut self,
        id: &ModuleId,
        task: JoinHandle<()>,
        limit: Duration,
    ) -> Result<(), String> {
        match wait_for(task, limit, &self.shutdown.expired).await {
            Waited::Done(()) => Ok(()),
            Waited::Panicked(payload) => {
                tracing::error!(module = %id, call = "drop", panic = %payload, "module instance drop panicked");
                Err(text::PANICKED.to_owned())
            }
            Waited::TimedOut(task) => {
                tracing::error!(module = %id, call = "drop", ?limit, "module instance drop did not finish in time");
                self.keep_pending(id, task);
                Err(text::TIMED_OUT.to_owned())
            }
            Waited::Interrupted(task) => {
                // The exit deadline, or the blocking task was cancelled
                // before it started (a `JoinError` without a panic).
                let error = if self.shutdown.expired.is_cancelled() {
                    tracing::error!(module = %id, call = "drop", "module instance drop not confirmed: the exit deadline has passed");
                    text::SHUTDOWN_DEADLINE
                } else {
                    tracing::error!(module = %id, call = "drop", "module instance drop was cancelled before it ran");
                    text::TIMED_OUT
                };
                self.keep_pending(id, task);
                Err(error.to_owned())
            }
        }
    }

    /// Remembers a drop that may still run; finished ones are forgotten.
    fn keep_pending(&mut self, id: &ModuleId, task: JoinHandle<()>) {
        self.pending_drops
            .retain(|pending| !pending.task.is_finished());
        if !task.is_finished() {
            self.pending_drops.push(PendingDrop {
                module: id.clone(),
                task,
            });
        }
    }

    /// Stops a running module: `Stopping`, then `then` on success or
    /// `Failed { phase: Stop }`. Does nothing for a module that is not running.
    async fn stop_running(&mut self, id: &ModuleId, then: ModuleState) -> Result<(), String> {
        let Some(instance) = self.running.remove(id) else {
            return Ok(());
        };
        self.health.remove(id);
        self.set_state(id, ModuleState::Stopping);
        match self.release(id, instance).await {
            Ok(()) => {
                self.set_state(id, then);
                Ok(())
            }
            Err(error) => {
                self.set_state(id, failed(FailurePhase::Stop, &error));
                Err(error)
            }
        }
    }

    /// Stop sequence of a started instance (MM-4 order): cancel the token →
    /// `stop` → drop the instance → wait for the background tasks → abort the
    /// rest. `stop` and the drop share one `STOP_TIMEOUT` (D1); the drop runs
    /// off the manager task. The first error is kept: `stop`, then the drop,
    /// then aborted tasks.
    async fn release(&mut self, id: &ModuleId, instance: Instance) -> Result<(), String> {
        let Instance { context, slot } = instance;
        context.cancel();
        let mut error = None;
        match self.reclaim(id, slot).await {
            Some(module) => {
                let budget = Instant::now() + STOP_TIMEOUT;
                let stopped = match self.call(id, module, &context, Lifecycle::Stop).await {
                    Ok(stopped) => Some(stopped),
                    Err(CallFailure {
                        error: stop_error,
                        instance,
                    }) => {
                        error = Some(stop_error);
                        instance
                    }
                };
                if let Some(module) = stopped {
                    let left = budget.saturating_duration_since(Instant::now());
                    if let Err(drop_error) = self.wait_drop(id, drop_off_task(module), left).await {
                        error.get_or_insert(drop_error);
                    }
                }
            }
            None => error = Some(text::UNRESPONSIVE.to_owned()),
        }
        let aborted = context
            .finish_tasks(TASKS_STOP_TIMEOUT, &self.shutdown.expired)
            .await;
        if aborted > 0 {
            tracing::warn!(module = %id, aborted, "background tasks did not finish in time and were aborted");
            error.get_or_insert_with(|| text::TASKS_ABORTED.to_owned());
        }
        error.map_or(Ok(()), Err)
    }

    /// The instance of a running module. A health check that did not answer in
    /// time may still hold it: wait for it within the stop limit.
    async fn reclaim(&self, id: &ModuleId, slot: Slot) -> Option<Box<dyn Module>> {
        let task = match slot {
            Slot::Ready(module) => return Some(module),
            Slot::Lost => return None,
            Slot::Checking(task) => task,
        };
        match wait_for(task, STOP_TIMEOUT, &self.shutdown.expired).await {
            Waited::Done((module, _late_result)) => Some(module),
            Waited::Panicked(payload) => {
                tracing::error!(module = %id, panic = %payload, "health check panicked");
                None
            }
            Waited::TimedOut(task) | Waited::Interrupted(task) => {
                task.abort();
                tracing::warn!(module = %id, "a health check still holds the instance: stopping without `stop`");
                None
            }
        }
    }

    /// A supervised background task of a running instance failed.
    ///
    /// After the exit request the report is only logged (D2): no status
    /// change, no stop, no cascade; `shutdown_all` stops the module and its
    /// result decides the final status.
    async fn task_failed(
        &mut self,
        id: &ModuleId,
        instance: u64,
        task: &'static str,
        failure: TaskFailure,
    ) {
        let current = self
            .running
            .get(id)
            .map(|running| running.context.instance());
        if self.shutdown.requested.is_cancelled() {
            log_exit_report(id, instance, task, &failure, current == Some(instance));
            return;
        }
        if current != Some(instance) {
            tracing::debug!(module = %id, task, error = %failure, "ignoring a report of an instance that is gone");
            return;
        }
        let error = match failure {
            TaskFailure::Error(failure) => {
                tracing::error!(module = %id, task, error = %failure, "background task failed");
                failure.message().to_owned()
            }
            TaskFailure::Panic(payload) => {
                tracing::error!(module = %id, task, panic = %payload, "background task panicked");
                text::PANICKED.to_owned()
            }
        };
        if let Some(instance) = self.running.remove(id) {
            self.fail(id, instance, FailurePhase::Run, error).await;
        }
    }

    /// A running module failed (a background task, or a health check panic):
    /// mark it `Failed` at once, release the instance, then stop its running
    /// dependents in reverse dependency order and block them (Q1, decision 9).
    async fn fail(
        &mut self,
        id: &ModuleId,
        instance: Instance,
        phase: FailurePhase,
        error: String,
    ) {
        self.health.remove(id);
        self.set_state(id, failed(phase, &error));
        if let Err(cleanup) = self.release(id, instance).await {
            tracing::warn!(module = %id, error = %cleanup, "the failed instance did not stop cleanly");
        }
        self.block_dependents(id).await;
    }

    /// Stops the running transitive dependents of `root` in reverse dependency
    /// order. They end up `Blocked { by: root, DependencyFailed }`, or
    /// `Failed { phase: Stop }` if their own stop fails. They stay enabled and
    /// start again only by a confirmed `recover`.
    async fn block_dependents(&mut self, root: &ModuleId) {
        let dependents: BTreeSet<ModuleId> = self
            .graph
            .transitive_dependents(root)
            .into_iter()
            .filter(|module| self.running.contains_key(module))
            .collect();
        for id in self.graph.in_shutdown_order(&dependents) {
            let blocked = ModuleState::Blocked {
                by: root.clone(),
                reason: BlockReason::DependencyFailed,
            };
            if let Err(error) = self.stop_running(&id, blocked).await {
                tracing::warn!(module = %id, %error, "a dependent of a failed module did not stop cleanly");
            }
        }
    }

    async fn check_health(&mut self, id: &ModuleId) -> Result<Health, ManagerError> {
        if self.shutdown.requested.is_cancelled() {
            return Err(ManagerError::ShutDown);
        }
        if !self.graph.contains(id) {
            return Err(ModuleError::UnknownModule(id.clone()).into());
        }
        let Some(Instance { context, slot }) = self.running.remove(id) else {
            return Err(ManagerError::NotRunning(id.clone()));
        };
        let module = match slot {
            Slot::Ready(module) => module,
            slot => {
                // The previous check still holds the instance: no second check.
                self.running.insert(id.clone(), Instance { context, slot });
                return Ok(Health::Unhealthy(text::HEALTH_NO_ANSWER.to_owned()));
            }
        };
        let task = spawn_health_check(module, &context, self.reports.clone());
        match wait_for(task, HEALTH_CHECK_TIMEOUT, &self.shutdown.requested).await {
            Waited::Done((module, health)) => {
                let slot = Slot::Ready(module);
                self.running.insert(id.clone(), Instance { context, slot });
                self.record_health(id, health.clone());
                Ok(health)
            }
            Waited::Panicked(payload) => {
                tracing::error!(module = %id, panic = %payload, "health check panicked");
                let health = Health::Unhealthy(text::HEALTH_PANICKED.to_owned());
                self.record_health(id, health.clone());
                let lost = Instance {
                    context,
                    slot: Slot::Lost,
                };
                self.fail(
                    id,
                    lost,
                    FailurePhase::HealthCheck,
                    text::PANICKED.to_owned(),
                )
                .await;
                Ok(health)
            }
            Waited::TimedOut(task) => {
                tracing::warn!(module = %id, "health check did not answer in time");
                let slot = Slot::Checking(task);
                self.running.insert(id.clone(), Instance { context, slot });
                let health = Health::Unhealthy(text::HEALTH_NO_ANSWER.to_owned());
                self.record_health(id, health.clone());
                Ok(health)
            }
            Waited::Interrupted(task) => {
                let slot = Slot::Checking(task);
                self.running.insert(id.clone(), Instance { context, slot });
                Err(ManagerError::ShutDown)
            }
        }
    }

    /// A health check task ended. If it is a late check that still held the
    /// instance, take the instance back, or fail the module if the check
    /// panicked.
    async fn health_check_ended(&mut self, id: &ModuleId, instance: u64) {
        let late = self.running.get(id).is_some_and(|running| {
            running.context.instance() == instance && matches!(running.slot, Slot::Checking(_))
        });
        if !late {
            return;
        }
        let Some(Instance { context, slot }) = self.running.remove(id) else {
            return;
        };
        let task = match slot {
            Slot::Checking(task) => task,
            slot => {
                self.running.insert(id.clone(), Instance { context, slot });
                return;
            }
        };
        match wait_for(task, HEALTH_CHECK_TIMEOUT, &self.shutdown.requested).await {
            Waited::Done((module, health)) => {
                let slot = Slot::Ready(module);
                self.running.insert(id.clone(), Instance { context, slot });
                self.record_health(id, health);
            }
            Waited::Panicked(payload) => {
                tracing::error!(module = %id, panic = %payload, "late health check panicked");
                self.record_health(id, Health::Unhealthy(text::HEALTH_PANICKED.to_owned()));
                let lost = Instance {
                    context,
                    slot: Slot::Lost,
                };
                self.fail(
                    id,
                    lost,
                    FailurePhase::HealthCheck,
                    text::PANICKED.to_owned(),
                )
                .await;
            }
            Waited::TimedOut(task) | Waited::Interrupted(task) => {
                let slot = Slot::Checking(task);
                self.running.insert(id.clone(), Instance { context, slot });
            }
        }
    }

    /// Stops all running modules in reverse dependency order within the exit
    /// deadline. When the deadline has passed, the modules left are
    /// terminated without `stop`: the instance is dropped off the manager task
    /// without waiting and the background tasks are aborted; they end up
    /// `Failed { phase: Stop }`. Drops not confirmed by then are logged. Every
    /// status change is published before the manager replies.
    async fn shutdown(&mut self) -> StopReport {
        let order: Vec<ModuleId> = self
            .graph
            .shutdown_order()
            .into_iter()
            .filter(|id| self.running.contains_key(id))
            .collect();
        tracing::info!(
            modules = order.len(),
            "stopping modules: the application is exiting"
        );
        let mut report = StopReport::default();
        for id in order {
            if self.shutdown.expired.is_cancelled() {
                let error = self.terminate(&id);
                report.failed.push(FailedModule { id, error });
                continue;
            }
            match self.stop_running(&id, ModuleState::Stopped).await {
                Ok(()) => report.stopped.push(id),
                Err(error) => report.failed.push(FailedModule { id, error }),
            }
        }
        for failure in &report.failed {
            tracing::error!(module = %failure.id, error = %failure.error, "module did not stop cleanly at exit");
        }
        tracing::info!(
            stopped = report.stopped.len(),
            failed = report.failed.len(),
            "modules stopped for exit"
        );
        self.pending_drops
            .retain(|pending| !pending.task.is_finished());
        if !self.pending_drops.is_empty() {
            let modules: Vec<String> = self
                .pending_drops
                .iter()
                .map(|pending| pending.module.to_string())
                .collect();
            tracing::warn!(
                modules = %modules.join(", "),
                "module instance drops not confirmed at exit: they may still run"
            );
        }
        report
    }

    /// The exit deadline has passed before this module's turn: no waiting.
    fn terminate(&mut self, id: &ModuleId) -> String {
        if let Some(Instance { context, slot }) = self.running.remove(id) {
            context.cancel();
            match slot {
                // Dropped off the manager task and not waited for (D1).
                Slot::Ready(module) => {
                    let task = drop_off_task(module);
                    self.keep_pending(id, task);
                }
                Slot::Checking(task) => task.abort(),
                Slot::Lost => {}
            }
            let aborted = context.abort_tasks();
            tracing::error!(module = %id, aborted, "module terminated: the exit deadline has passed");
        }
        self.health.remove(id);
        let error = text::SHUTDOWN_DEADLINE.to_owned();
        self.set_state(id, failed(FailurePhase::Stop, &error));
        error
    }

    fn record_health(&mut self, id: &ModuleId, health: Health) {
        if self.health.get(id) == Some(&health) {
            return;
        }
        self.health.insert(id.clone(), health.clone());
        self.publish(ModuleEvent::HealthChanged {
            id: id.clone(),
            health,
        });
    }

    fn state(&self, id: &ModuleId) -> ModuleState {
        self.statuses
            .get(id)
            .map_or(ModuleState::Stopped, |status| status.state.clone())
    }

    fn set_enabled(&mut self, id: &ModuleId, enabled: bool) {
        let state = self.state(id);
        self.set_status(id, ModuleStatus { enabled, state });
    }

    fn set_state(&mut self, id: &ModuleId, state: ModuleState) {
        let enabled = self.statuses.get(id).is_some_and(|status| status.enabled);
        self.set_status(id, ModuleStatus { enabled, state });
    }

    /// The only place where a status changes: the registry and the snapshot
    /// are updated first, then the change is announced on the bus, so a
    /// subscriber that reads the snapshot sees at least this change.
    fn set_status(&mut self, id: &ModuleId, current: ModuleStatus) {
        let previous = self.statuses.get(id).cloned().unwrap_or_default();
        if previous == current {
            return;
        }
        if !current.is_consistent() {
            tracing::error!(module = %id, status = ?current, "inconsistent module status");
        }
        debug_assert!(
            current.is_consistent(),
            "inconsistent status of module `{id}`: {current:?}"
        );
        self.statuses.insert(id.clone(), current.clone());
        self.snapshot.send_modify(|snapshot| {
            snapshot.insert(id.clone(), current.clone());
        });
        tracing::info!(module = %id, enabled = current.enabled, state = ?current.state, "module status changed");
        self.publish(ModuleEvent::StatusChanged {
            id: id.clone(),
            previous,
            current,
        });
    }

    fn publish(&self, event: ModuleEvent) {
        if self.events.publish(AppEvent::Module(event)).is_err() {
            tracing::error!("module event not published: the event bus is closed");
        }
    }
}

fn failed(phase: FailurePhase, error: &str) -> ModuleState {
    ModuleState::Failed {
        phase,
        error: error.to_owned(),
        at: unix_time_ms(),
    }
}

#[derive(Debug, Clone, Copy)]
enum Lifecycle {
    Initialize,
    Start,
    Stop,
}

impl Lifecycle {
    fn name(self) -> &'static str {
        match self {
            Self::Initialize => "initialize",
            Self::Start => "start",
            Self::Stop => "stop",
        }
    }
}

/// One message taken from the queue during the exit: nothing is executed.
/// Reports are only logged, commands get `ShutDown`, the end of a late health
/// check needs nothing (`shutdown_all` has taken the instance back or
/// terminated the module), and repeated exit requests are kept until the end
/// is logged.
fn exit_message(
    command: Command,
    running_at_exit: &BTreeSet<(ModuleId, u64)>,
    repeated: &mut Vec<oneshot::Sender<StopReport>>,
) {
    match command {
        Command::Apply { reply, .. } => {
            let _ = reply.send(Err(ManagerError::ShutDown));
        }
        Command::CheckHealth { reply, .. } => {
            let _ = reply.send(Err(ManagerError::ShutDown));
        }
        Command::TaskFailed {
            id,
            instance,
            task,
            failure,
        } => {
            let running = running_at_exit.contains(&(id.clone(), instance));
            log_exit_report(&id, instance, task, &failure, running);
        }
        Command::HealthCheckEnded { .. } => {}
        Command::Shutdown { reply } => repeated.push(reply),
    }
}

/// A background task failure reported after the exit request: only logged
/// (D2). `warn` for an instance still running when the exit began, `debug`
/// for one stopped before; the text is kept in both.
fn log_exit_report(
    id: &ModuleId,
    instance: u64,
    task: &'static str,
    failure: &TaskFailure,
    running_at_exit: bool,
) {
    if running_at_exit {
        tracing::warn!(module = %id, instance, task, error = %failure, "background task failed after the exit request");
    } else {
        tracing::debug!(module = %id, instance, task, error = %failure, "background task of a stopped instance failed after the exit request");
    }
}

impl CallFailure {
    fn without_instance(error: &str) -> Self {
        Self {
            error: error.to_owned(),
            instance: None,
        }
    }
}

/// Drops a module instance on the blocking pool, never in the manager task
/// (D1): a panic in `Drop` stays in that task, and a `Drop` that blocks holds
/// only a pool thread. A blocking task that has started cannot be aborted.
fn drop_off_task(module: Box<dyn Module>) -> JoinHandle<()> {
    tokio::task::spawn_blocking(move || drop(module))
}

/// Moves the instance into its own task for one call and returns it from there.
fn spawn_call(
    mut module: Box<dyn Module>,
    context: ModuleContext,
    call: Lifecycle,
) -> JoinHandle<(Box<dyn Module>, Result<(), ModuleFailure>)> {
    tokio::spawn(async move {
        let result = match call {
            Lifecycle::Initialize => module.initialize(&context).await,
            Lifecycle::Start => module.start(&context).await,
            Lifecycle::Stop => module.stop(&context).await,
        };
        (module, result)
    })
}

/// Moves the instance into its own task for a health check. When the task
/// ends, also by a panic or an abort, the manager is told.
fn spawn_health_check(
    module: Box<dyn Module>,
    context: &ModuleContext,
    reports: WeakUnboundedSender<Command>,
) -> JoinHandle<(Box<dyn Module>, Health)> {
    let ended = HealthCheckEnded {
        id: context.id().clone(),
        instance: context.instance(),
        reports,
    };
    tokio::spawn(async move {
        let _ended = ended;
        let health = module.health_check().await;
        (module, health)
    })
}

/// Sends `Command::HealthCheckEnded` when dropped.
struct HealthCheckEnded {
    id: ModuleId,
    instance: u64,
    reports: WeakUnboundedSender<Command>,
}

impl Drop for HealthCheckEnded {
    fn drop(&mut self) {
        if let Some(reports) = self.reports.upgrade() {
            let _ = reports.send(Command::HealthCheckEnded {
                id: self.id.clone(),
                instance: self.instance,
            });
        }
    }
}

enum Waited<T> {
    Done(T),
    Panicked(String),
    TimedOut(JoinHandle<T>),
    Interrupted(JoinHandle<T>),
}

/// Waits for `task` at most `limit` and not after `stop_waiting` is
/// cancelled. A task that has not finished is handed back: the caller decides
/// whether to abort it or keep it.
async fn wait_for<T>(
    mut task: JoinHandle<T>,
    limit: Duration,
    stop_waiting: &CancellationToken,
) -> Waited<T> {
    let result = stop_waiting
        .run_until_cancelled(timeout(limit, &mut task))
        .await;
    match result {
        Some(Ok(Ok(value))) => Waited::Done(value),
        Some(Ok(Err(error))) if error.is_panic() => {
            Waited::Panicked(panic_text(error.into_panic()))
        }
        // Cancelled from outside, e.g. while the runtime shuts down.
        Some(Ok(Err(_))) | None => Waited::Interrupted(task),
        Some(Err(_elapsed)) => Waited::TimedOut(task),
    }
}

/// Test hook (as in `events::bus`): lets a test observe the moment right
/// after the reply to `Shutdown`, on the manager's thread (test 57).
#[cfg(test)]
pub(super) mod test_hooks {
    use std::cell::RefCell;

    type AfterReply = Option<Box<dyn FnMut()>>;

    thread_local! {
        /// Called by the manager on this thread right after it has sent the
        /// reply to `Shutdown`.
        pub(in crate::modules) static AFTER_REPLY: RefCell<AfterReply> =
            const { RefCell::new(None) };
    }

    pub(super) fn after_reply() {
        AFTER_REPLY.with_borrow_mut(|hook| {
            if let Some(hook) = hook {
                hook();
            }
        });
    }
}
