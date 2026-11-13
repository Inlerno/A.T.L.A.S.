//! What a module instance can use (issue #9, ADR-014).
//!
//! A context belongs to one instance. It gives the module its settings (#10),
//! a subscription to the event bus, a cancellation token and supervised
//! background tasks. It
//! gives no access to the module manager (a module waiting for the manager
//! while the manager waits for the module would deadlock) and no way to
//! publish events (#12).

use std::any::Any;
use std::fmt;
use std::future::Future;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use serde_json::Value;
use tokio::sync::mpsc::WeakUnboundedSender;
use tokio::task::AbortHandle;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use super::contract::{ModuleFailure, ModuleId};
use super::manager::Command;
use crate::events::bus::{EventBus, Subscriber};

/// How a supervised background task ended badly.
// Module-facing API (ADR-014): its first consumers are the demo modules of
// #12; until then only tests use it.
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug)]
pub(super) enum TaskFailure {
    /// The task returned an error; its message is safe to show.
    Error(ModuleFailure),
    /// The task panicked; the payload text goes to the log only.
    Panic(String),
}

/// For the log: the error text, or the panic text.
impl fmt::Display for TaskFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Error(failure) => write!(f, "{failure}"),
            Self::Panic(payload) => write!(f, "panic: {payload}"),
        }
    }
}

/// Handle given to a module instance. Cloning is cheap; all clones belong to
/// the same instance.
#[derive(Clone)]
pub struct ModuleContext {
    inner: Arc<Inner>,
}

struct Inner {
    module: ModuleId,
    /// Number of the instance, so that reports of an older instance are ignored.
    instance: u64,
    /// Settings the instance was created with; they do not change while it
    /// runs (#10).
    settings: Arc<Value>,
    // Module-facing API (ADR-014): its first consumers are the demo modules of
    // #12; until then only tests use it.
    #[cfg_attr(not(test), allow(dead_code))]
    events: EventBus,
    /// Cancelled by the manager when the instance stops.
    token: CancellationToken,
    tracker: TaskTracker,
    /// Abort handles of the background tasks; `None` once the instance has
    /// been stopped, after which no new task starts.
    tasks: Mutex<Option<Vec<AbortHandle>>>,
    /// Failure reports go to the manager as commands. Weak: a context does not
    /// keep the manager alive.
    // Module-facing API (ADR-014): its first consumers are the demo modules of
    // #12; until then only tests use it.
    #[cfg_attr(not(test), allow(dead_code))]
    reports: WeakUnboundedSender<Command>,
}

impl ModuleContext {
    pub(super) fn new(
        module: ModuleId,
        instance: u64,
        settings: Arc<Value>,
        events: EventBus,
        reports: WeakUnboundedSender<Command>,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                module,
                instance,
                settings,
                events,
                token: CancellationToken::new(),
                tracker: TaskTracker::new(),
                tasks: Mutex::new(Some(Vec::new())),
                reports,
            }),
        }
    }

    /// Identifier of the module this instance belongs to.
    pub fn id(&self) -> &ModuleId {
        &self.inner.module
    }

    /// Settings of this instance, already checked by the manager and the
    /// module's factory. Available from `initialize` on and the same for the
    /// whole life of the instance: new settings apply to the next instance,
    /// after a confirmed `restart` (#10). The module deserializes them into
    /// its own type.
    // Module-facing API (ADR-014): its first consumers are the demo modules of
    // #12; until then only tests use it.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn settings(&self) -> &Value {
        &self.inner.settings
    }

    pub(super) fn settings_arc(&self) -> &Arc<Value> {
        &self.inner.settings
    }

    /// Subscribes to the central event bus. Modules only subscribe; publishing
    /// by modules arrives in #12.
    // Module-facing API (ADR-014): its first consumers are the demo modules of
    // #12; until then only tests use it.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn subscribe(&self) -> Subscriber {
        self.inner.events.subscribe()
    }

    /// Token cancelled when the instance is being stopped. Background tasks
    /// watch it and finish promptly.
    // Module-facing API (ADR-014): its first consumers are the demo modules of
    // #12; until then only tests use it.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn cancellation_token(&self) -> CancellationToken {
        self.inner.token.child_token()
    }

    /// Runs `task` in the background under the manager's supervision.
    ///
    /// If the task returns an error or panics, the module fails with
    /// `Failed { phase: Run }` and its running dependents are blocked. Once
    /// the application has started to exit, the failure is only logged and
    /// the module is stopped by the exit like the others. When the
    /// instance stops, the manager waits for its tasks for a limited time and
    /// then aborts the rest. A task started after the stop has begun is not
    /// run. Must be called within the tokio runtime, as all module code is.
    // Module-facing API (ADR-014): its first consumers are the demo modules of
    // #12; until then only tests use it.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn spawn<F>(&self, name: &'static str, task: F)
    where
        F: Future<Output = Result<(), ModuleFailure>> + Send + 'static,
    {
        let inner = &self.inner;
        let mut tasks = lock(&inner.tasks);
        let Some(handles) = tasks.as_mut() else {
            tracing::warn!(
                module = %inner.module,
                task = name,
                "background task not started: the module is stopping"
            );
            return;
        };
        let work = inner.tracker.spawn(task);
        handles.push(work.abort_handle());
        let module = inner.module.clone();
        let instance = inner.instance;
        let reports = inner.reports.clone();
        inner.tracker.spawn(async move {
            let failure = match work.await {
                Ok(Ok(())) => return,
                Ok(Err(failure)) => TaskFailure::Error(failure),
                Err(error) if error.is_panic() => {
                    TaskFailure::Panic(panic_text(error.into_panic()))
                }
                // Aborted by the manager while the instance was stopping.
                Err(_) => return,
            };
            let report = Command::TaskFailed {
                id: module,
                instance,
                task: name,
                failure,
            };
            // The manager closes its queue when the application exits and is
            // gone after that: then the supervisor logs the failure itself, so
            // it is never lost silently (D2).
            let undelivered = match reports.upgrade() {
                Some(reports) => reports.send(report).err().map(|error| error.0),
                None => Some(report),
            };
            if let Some(Command::TaskFailed {
                id,
                instance,
                task,
                failure,
            }) = undelivered
            {
                tracing::warn!(
                    module = %id,
                    instance,
                    task,
                    error = %failure,
                    "background task failed, but the module manager can no longer be told"
                );
            }
        });
    }

    pub(super) fn instance(&self) -> u64 {
        self.inner.instance
    }

    /// Asks the instance's background tasks to finish.
    pub(super) fn cancel(&self) {
        self.inner.token.cancel();
    }

    /// Waits for the background tasks to finish, at most `limit` and not after
    /// `deadline` is cancelled, then aborts the tasks still running. Returns
    /// how many tasks had to be aborted.
    pub(super) async fn finish_tasks(
        &self,
        limit: Duration,
        deadline: &CancellationToken,
    ) -> usize {
        self.inner.tracker.close();
        let _ = deadline
            .run_until_cancelled(timeout(limit, self.inner.tracker.wait()))
            .await;
        self.abort_tasks()
    }

    /// Aborts the background tasks still running and refuses new ones. Returns
    /// how many tasks were still running.
    pub(super) fn abort_tasks(&self) -> usize {
        self.inner.tracker.close();
        let handles = lock(&self.inner.tasks).take().unwrap_or_default();
        let running = handles
            .iter()
            .filter(|handle| !handle.is_finished())
            .count();
        for handle in handles {
            handle.abort();
        }
        running
    }

    /// Tracked tasks still alive (background tasks and their supervisors).
    #[cfg(test)]
    pub(super) fn tasks_alive(&self) -> usize {
        self.inner.tracker.len()
    }
}

/// The task list stays consistent if a holder panicked: recover the lock.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Text of a panic payload, for the log only.
pub(super) fn panic_text(payload: Box<dyn Any + Send>) -> String {
    if let Some(text) = payload.downcast_ref::<&str>() {
        (*text).to_owned()
    } else if let Some(text) = payload.downcast_ref::<String>() {
        text.clone()
    } else {
        "non-text panic payload".to_owned()
    }
}
