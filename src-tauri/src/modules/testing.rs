//! Test doubles for the lifecycle manager (issue #9).
//!
//! `FakeFactory` creates `FakeModule` instances whose behaviour in each phase
//! is scripted (succeed, fail, panic, hang). A shared `Probe` counts created
//! and dropped instances and calls, and can end the background task of the
//! current instance on demand. Panic messages contain a fake path so tests
//! can check that such details stay out of user-visible texts.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use tokio::sync::oneshot;

use super::context::ModuleContext;
use super::contract::{
    BoxFuture, Health, Module, ModuleDescriptor, ModuleFactory, ModuleFailure, ModuleId,
};
use crate::events::bus::Subscriber;

/// Behaviour of a lifecycle call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Behavior {
    Succeed,
    Fail,
    Panic,
    /// Never returns (but yields, so a timeout can end it).
    Hang,
    /// Succeeds after the given time.
    Delay(Duration),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HealthBehavior {
    Answer(Health),
    Panic,
    Hang,
    /// Answers `Healthy`, or panics, after the given time.
    Late {
        after: Duration,
        panic: bool,
    },
}

/// What the background task spawned by `start` does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Background {
    None,
    /// Finishes when cancelled or when the test ends it.
    Cooperative,
    /// Ignores cancellation: only an abort ends it.
    IgnoresCancellation,
    /// Returns an error when cancelled, i.e. while the instance stops.
    FailsOnCancel,
}

#[derive(Debug, Clone)]
pub struct Script {
    pub initialize: Behavior,
    pub start: Behavior,
    pub stop: Behavior,
    pub health: HealthBehavior,
    pub background: Background,
}

impl Default for Script {
    fn default() -> Self {
        Self {
            initialize: Behavior::Succeed,
            start: Behavior::Succeed,
            stop: Behavior::Succeed,
            health: HealthBehavior::Answer(Health::Healthy),
            background: Background::Cooperative,
        }
    }
}

/// How the test ends the background task of the current instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskEnd {
    Fail,
    Panic,
}

/// What a test sees and controls for one fake module.
#[derive(Default)]
pub struct Probe {
    pub created: AtomicUsize,
    pub dropped: AtomicUsize,
    pub initialized: AtomicUsize,
    pub started: AtomicUsize,
    pub stopped: AtomicUsize,
    pub health_checks: AtomicUsize,
    /// Background tasks currently alive.
    pub background: AtomicUsize,
    script: Mutex<Script>,
    trigger: Mutex<Option<oneshot::Sender<TaskEnd>>>,
    context: Mutex<Option<ModuleContext>>,
    subscriber: Mutex<Option<Subscriber>>,
}

impl Probe {
    pub fn count(counter: &AtomicUsize) -> usize {
        counter.load(Ordering::SeqCst)
    }

    /// Changes the behaviour of the following calls.
    pub fn script(&self, change: impl FnOnce(&mut Script)) {
        change(&mut lock(&self.script));
    }

    /// Ends the background task of the current instance. Returns `false` if
    /// there is none.
    pub fn end_background(&self, end: TaskEnd) -> bool {
        lock(&self.trigger)
            .take()
            .is_some_and(|trigger| trigger.send(end).is_ok())
    }

    /// Context of the latest instance.
    pub fn context(&self) -> Option<ModuleContext> {
        lock(&self.context).clone()
    }

    /// Subscription the latest instance took in `initialize`.
    pub fn take_subscriber(&self) -> Option<Subscriber> {
        lock(&self.subscriber).take()
    }

    fn current(&self) -> Script {
        lock(&self.script).clone()
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

pub struct FakeFactory {
    descriptor: ModuleDescriptor,
    probe: Arc<Probe>,
}

impl FakeFactory {
    pub fn new(id: &str, dependencies: &[&str]) -> (Self, Arc<Probe>) {
        let module_id = |value: &str| ModuleId::new(value).expect("valid module id");
        let probe = Arc::new(Probe::default());
        let factory = Self {
            descriptor: ModuleDescriptor {
                id: module_id(id),
                display_name: id.to_owned(),
                description: String::new(),
                version: "0.0.0".to_owned(),
                dependencies: dependencies.iter().map(|d| module_id(d)).collect(),
                default_enabled: false,
            },
            probe: Arc::clone(&probe),
        };
        (factory, probe)
    }
}

impl ModuleFactory for FakeFactory {
    fn descriptor(&self) -> &ModuleDescriptor {
        &self.descriptor
    }

    fn create(&self) -> Box<dyn Module> {
        self.probe.created.fetch_add(1, Ordering::SeqCst);
        Box::new(FakeModule {
            probe: Arc::clone(&self.probe),
        })
    }
}

struct FakeModule {
    probe: Arc<Probe>,
}

impl Drop for FakeModule {
    fn drop(&mut self) {
        self.probe.dropped.fetch_add(1, Ordering::SeqCst);
    }
}

async fn act(behavior: Behavior, call: &str) -> Result<(), ModuleFailure> {
    match behavior {
        Behavior::Succeed => Ok(()),
        Behavior::Fail => Err(ModuleFailure::new(format!("fake {call} error"))),
        Behavior::Panic => panic!("fake {call} panic in /secret/path/{call}.rs"),
        Behavior::Hang => std::future::pending().await,
        Behavior::Delay(time) => {
            tokio::time::sleep(time).await;
            Ok(())
        }
    }
}

/// Counts a live background task until dropped (also when aborted).
struct Alive(Arc<Probe>);

impl Alive {
    fn new(probe: &Arc<Probe>) -> Self {
        probe.background.fetch_add(1, Ordering::SeqCst);
        Self(Arc::clone(probe))
    }
}

impl Drop for Alive {
    fn drop(&mut self) {
        self.0.background.fetch_sub(1, Ordering::SeqCst);
    }
}

impl Module for FakeModule {
    fn initialize<'a>(
        &'a mut self,
        ctx: &'a ModuleContext,
    ) -> BoxFuture<'a, Result<(), ModuleFailure>> {
        Box::pin(async move {
            self.probe.initialized.fetch_add(1, Ordering::SeqCst);
            *lock(&self.probe.context) = Some(ctx.clone());
            *lock(&self.probe.subscriber) = Some(ctx.subscribe());
            act(self.probe.current().initialize, "initialize").await
        })
    }

    fn start<'a>(&'a mut self, ctx: &'a ModuleContext) -> BoxFuture<'a, Result<(), ModuleFailure>> {
        Box::pin(async move {
            self.probe.started.fetch_add(1, Ordering::SeqCst);
            let script = self.probe.current();
            if script.background != Background::None {
                let (trigger, ended) = oneshot::channel();
                *lock(&self.probe.trigger) = Some(trigger);
                let token = ctx.cancellation_token();
                let alive = Alive::new(&self.probe);
                let background = script.background;
                ctx.spawn("fake work", async move {
                    let _alive = alive;
                    if background == Background::IgnoresCancellation {
                        std::future::pending::<()>().await;
                    }
                    if background == Background::FailsOnCancel {
                        token.cancelled().await;
                        return Err(ModuleFailure::new("fake error while stopping"));
                    }
                    match token.run_until_cancelled(ended).await {
                        Some(Ok(TaskEnd::Fail)) => Err(ModuleFailure::new("fake background error")),
                        Some(Ok(TaskEnd::Panic)) => {
                            panic!("fake background panic in /secret/path/work.rs")
                        }
                        Some(Err(_)) | None => Ok(()),
                    }
                });
            }
            act(script.start, "start").await
        })
    }

    fn stop<'a>(&'a mut self, _ctx: &'a ModuleContext) -> BoxFuture<'a, Result<(), ModuleFailure>> {
        Box::pin(async move {
            self.probe.stopped.fetch_add(1, Ordering::SeqCst);
            act(self.probe.current().stop, "stop").await
        })
    }

    fn health_check(&self) -> BoxFuture<'_, Health> {
        Box::pin(async move {
            self.probe.health_checks.fetch_add(1, Ordering::SeqCst);
            match self.probe.current().health {
                HealthBehavior::Answer(health) => health,
                HealthBehavior::Panic => panic!("fake health panic in /secret/path/health.rs"),
                HealthBehavior::Hang => std::future::pending().await,
                HealthBehavior::Late { after, panic } => {
                    tokio::time::sleep(after).await;
                    assert!(!panic, "fake late health panic in /secret/path/health.rs");
                    Health::Healthy
                }
            }
        })
    }
}
