//! Test doubles and the shared harness for the lifecycle manager (issues #9
//! and #10).
//!
//! `FakeFactory` creates `FakeModule` instances whose behaviour in each phase
//! is scripted (succeed, fail, panic, hang), and so is dropping the instance
//! (normal, panic, block until the test releases it). Its settings methods are
//! scripted too (defaults, validator, panics). A shared `Probe` counts
//! created and dropped instances and calls, records the settings each
//! instance got, and can end the background task of the current instance on
//! demand. `TestStore` is the in-memory store with injected failures, hangs
//! and delays. Panic messages contain a fake path so tests can check that
//! such details stay out of user-visible texts.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, Write};
use std::pin::pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, Once, PoisonError};
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::sync::oneshot;

use super::config::{InMemoryConfigStore, ModuleConfig, ModuleConfigStore, StoreError};
use super::context::ModuleContext;
use super::contract::{
    BoxFuture, Health, Module, ModuleDescriptor, ModuleFactory, ModuleFailure, ModuleId,
    ModuleState, ModuleStatus,
};
use super::manager::{self, ApplyOutcome, ApplyReport, ManagerError, ModuleManagerHandle};
use super::plan::{ChangeAction, Confirmation};
use crate::events::bus::{EventBus, Received, Subscriber};
use crate::events::types::{AppEvent, ModuleEvent};

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
    /// Fails after the given time.
    FailAfter(Duration),
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

/// What dropping an instance does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DropBehavior {
    Normal,
    /// Panics, unless the thread is already unwinding (a second panic would
    /// abort the test binary).
    Panic,
    /// Blocks the thread until the test calls `Probe::release_drops`, or at
    /// most `DROP_BLOCK_LIMIT`, so a broken test cannot hang forever.
    Block,
}

/// Upper limit of `DropBehavior::Block`.
pub const DROP_BLOCK_LIMIT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone)]
pub struct Script {
    pub initialize: Behavior,
    pub start: Behavior,
    pub stop: Behavior,
    pub health: HealthBehavior,
    pub background: Background,
    pub drop: DropBehavior,
}

impl Default for Script {
    fn default() -> Self {
        Self {
            initialize: Behavior::Succeed,
            start: Behavior::Succeed,
            stop: Behavior::Succeed,
            health: HealthBehavior::Answer(Health::Healthy),
            background: Background::Cooperative,
            drop: DropBehavior::Normal,
        }
    }
}

/// How the test ends the background task of the current instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskEnd {
    Fail,
    Panic,
}

/// How the fake factory checks settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Validator {
    /// Accepts every object without the key `invalid`.
    RejectsInvalidKey,
    Panic,
}

/// Scripted settings methods and `create` of the fake factory.
#[derive(Debug, Clone)]
pub struct FactoryScript {
    pub defaults: Value,
    pub defaults_panic: bool,
    pub validator: Validator,
    pub create_panic: bool,
}

impl Default for FactoryScript {
    fn default() -> Self {
        Self {
            defaults: json!({}),
            defaults_panic: false,
            validator: Validator::RejectsInvalidKey,
            create_panic: false,
        }
    }
}

/// Message of a settings value rejected by the fake validator.
pub const FAKE_INVALID: &str = "fake: the value is invalid";

/// Runs once inside the next `initialize`, e.g. to request the exit.
type InitializeHook = Box<dyn FnOnce() + Send>;

/// What a test sees and controls for one fake module.
#[derive(Default)]
pub struct Probe {
    pub created: AtomicUsize,
    /// Drops begun (also those that panic or block).
    pub dropped: AtomicUsize,
    /// Drops that returned normally.
    pub drops_finished: AtomicUsize,
    pub initialized: AtomicUsize,
    pub started: AtomicUsize,
    pub stopped: AtomicUsize,
    pub health_checks: AtomicUsize,
    /// Background tasks currently alive.
    pub background: AtomicUsize,
    /// Value of `background` at the start of each drop, in order.
    pub background_at_drop: Mutex<Vec<usize>>,
    /// Set by `release_drops`: blocked drops return, later ones do not block.
    drops_released: Mutex<bool>,
    drop_released: Condvar,
    pub validations: AtomicUsize,
    /// Settings each instance found in its context in `initialize`, in order.
    pub settings_seen: Mutex<Vec<Value>>,
    script: Mutex<Script>,
    factory: Mutex<FactoryScript>,
    on_initialize: Mutex<Option<InitializeHook>>,
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

    /// Changes the settings methods and `create` of the factory.
    pub fn factory(&self, change: impl FnOnce(&mut FactoryScript)) {
        change(&mut lock(&self.factory));
    }

    /// Runs `hook` once inside the next `initialize`.
    pub fn on_initialize(&self, hook: impl FnOnce() + Send + 'static) {
        *lock(&self.on_initialize) = Some(Box::new(hook));
    }

    /// Settings seen by the instances, in order.
    pub fn settings_seen(&self) -> Vec<Value> {
        lock(&self.settings_seen).clone()
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

    /// Lets every blocked drop return; later drops of this module no longer
    /// block.
    pub fn release_drops(&self) {
        *lock(&self.drops_released) = true;
        self.drop_released.notify_all();
    }

    fn wait_for_release(&self) {
        let released = lock(&self.drops_released);
        let _released = self
            .drop_released
            .wait_timeout_while(released, DROP_BLOCK_LIMIT, |released| !*released)
            .unwrap_or_else(PoisonError::into_inner);
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
        Self::with_default(id, dependencies, false)
    }

    /// A fake module with the given `default_enabled`.
    pub fn with_default(
        id: &str,
        dependencies: &[&str],
        default_enabled: bool,
    ) -> (Self, Arc<Probe>) {
        let module_id = |value: &str| ModuleId::new(value).expect("valid module id");
        let probe = Arc::new(Probe::default());
        let factory = Self {
            descriptor: ModuleDescriptor {
                id: module_id(id),
                display_name: id.to_owned(),
                description: String::new(),
                version: "0.0.0".to_owned(),
                dependencies: dependencies.iter().map(|d| module_id(d)).collect(),
                default_enabled,
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
        assert!(
            !lock(&self.probe.factory).create_panic,
            "fake create panic in /secret/path/create.rs"
        );
        self.probe.created.fetch_add(1, Ordering::SeqCst);
        Box::new(FakeModule {
            probe: Arc::clone(&self.probe),
        })
    }

    fn default_settings(&self) -> Value {
        let script = lock(&self.probe.factory).clone();
        assert!(
            !script.defaults_panic,
            "fake default_settings panic in /secret/path/defaults.rs"
        );
        script.defaults
    }

    fn validate_settings(&self, settings: &Value) -> Result<(), ModuleFailure> {
        self.probe.validations.fetch_add(1, Ordering::SeqCst);
        let validator = lock(&self.probe.factory).validator;
        match validator {
            Validator::Panic => panic!("fake validator panic in /secret/path/validate.rs"),
            Validator::RejectsInvalidKey if settings.get("invalid").is_some() => {
                Err(ModuleFailure::new(FAKE_INVALID))
            }
            Validator::RejectsInvalidKey => Ok(()),
        }
    }
}

struct FakeModule {
    probe: Arc<Probe>,
}

impl Drop for FakeModule {
    fn drop(&mut self) {
        let probe = &self.probe;
        lock(&probe.background_at_drop).push(probe.background.load(Ordering::SeqCst));
        probe.dropped.fetch_add(1, Ordering::SeqCst);
        match probe.current().drop {
            DropBehavior::Normal => {}
            DropBehavior::Panic if std::thread::panicking() => {}
            DropBehavior::Panic => panic!("fake drop panic in /secret/path/drop.rs"),
            DropBehavior::Block => probe.wait_for_release(),
        }
        probe.drops_finished.fetch_add(1, Ordering::SeqCst);
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
        Behavior::FailAfter(time) => {
            tokio::time::sleep(time).await;
            Err(ModuleFailure::new(format!("fake {call} error")))
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
            lock(&self.probe.settings_seen).push(ctx.settings().clone());
            let hook = lock(&self.probe.on_initialize).take();
            if let Some(hook) = hook {
                hook();
            }
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

// ---------------------------------------------------------------------------
// Test store (#10)
// ---------------------------------------------------------------------------

/// Failures, hangs and delays injected into `TestStore`, and what it saw.
#[derive(Default)]
pub struct StoreControl {
    fail_load: Mutex<BTreeSet<ModuleId>>,
    hang_load: Mutex<BTreeSet<ModuleId>>,
    fail_save: AtomicBool,
    hang_save: AtomicBool,
    loads: Mutex<Vec<ModuleId>>,
    /// Every batch the manager tried to save: (module, enabled) pairs.
    batches: Mutex<Vec<Vec<(ModuleId, bool)>>>,
}

/// The in-memory store with injected failures. Clones share everything, so
/// a second manager can start on the same data.
#[derive(Clone, Default)]
pub struct TestStore {
    inner: InMemoryConfigStore,
    control: Arc<StoreControl>,
}

impl TestStore {
    /// Stores a record directly, as an earlier run would have.
    pub fn put(&self, module: &str, enabled: bool, settings: Value) {
        let batch = [(id(module), ModuleConfig { enabled, settings })];
        match poll_once(self.inner.save_batch(&batch)) {
            Poll::Ready(result) => result.expect("in-memory save"),
            Poll::Pending => unreachable!("the in-memory store never waits"),
        }
    }

    pub fn record(&self, module: &str) -> Option<ModuleConfig> {
        self.inner.get(&id(module))
    }

    pub fn records(&self) -> BTreeMap<ModuleId, ModuleConfig> {
        self.inner.records()
    }

    pub fn fail_load(&self, module: &str) {
        lock(&self.control.fail_load).insert(id(module));
    }

    pub fn hang_load(&self, module: &str) {
        lock(&self.control.hang_load).insert(id(module));
    }

    /// Reads work again (as in a later run, after the cause was removed).
    pub fn clear_failures(&self) {
        lock(&self.control.fail_load).clear();
        lock(&self.control.hang_load).clear();
    }

    pub fn fail_saves(&self, fail: bool) {
        self.control.fail_save.store(fail, Ordering::SeqCst);
    }

    pub fn hang_saves(&self, hang: bool) {
        self.control.hang_save.store(hang, Ordering::SeqCst);
    }

    /// Modules read so far, in order.
    pub fn loads(&self) -> Vec<String> {
        lock(&self.control.loads)
            .iter()
            .map(ToString::to_string)
            .collect()
    }

    /// Batches the manager tried to save, as (module, enabled) pairs.
    pub fn batches(&self) -> Vec<Vec<(String, bool)>> {
        lock(&self.control.batches)
            .iter()
            .map(|batch| {
                batch
                    .iter()
                    .map(|(id, enabled)| (id.to_string(), *enabled))
                    .collect()
            })
            .collect()
    }
}

impl ModuleConfigStore for TestStore {
    fn load<'a>(
        &'a self,
        id: &'a ModuleId,
    ) -> BoxFuture<'a, Result<Option<ModuleConfig>, StoreError>> {
        Box::pin(async move {
            lock(&self.control.loads).push(id.clone());
            if lock(&self.control.hang_load).contains(id) {
                std::future::pending::<()>().await;
            }
            if lock(&self.control.fail_load).contains(id) {
                return Err(StoreError(format!(
                    "fake read error in /secret/path/config.db for {id}"
                )));
            }
            self.inner.load(id).await
        })
    }

    fn save_batch<'a>(
        &'a self,
        entries: &'a [(ModuleId, ModuleConfig)],
    ) -> BoxFuture<'a, Result<(), StoreError>> {
        Box::pin(async move {
            lock(&self.control.batches).push(
                entries
                    .iter()
                    .map(|(id, config)| (id.clone(), config.enabled))
                    .collect(),
            );
            if self.control.hang_save.load(Ordering::SeqCst) {
                std::future::pending::<()>().await;
            }
            if self.control.fail_save.load(Ordering::SeqCst) {
                return Err(StoreError(
                    "fake write error in /secret/path/config.db".to_owned(),
                ));
            }
            // Atomic: the inner save never waits.
            self.inner.save_batch(entries).await
        })
    }
}

// ---------------------------------------------------------------------------
// Harness shared by the manager tests (#9, #10)
// ---------------------------------------------------------------------------

pub fn run<F: Future>(test: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .start_paused(true)
        .build()
        .expect("test runtime")
        .block_on(test)
}

pub fn id(value: &str) -> ModuleId {
    ModuleId::new(value).expect("valid module id")
}

pub fn poll_once<F: Future>(future: F) -> Poll<F::Output> {
    pin!(future).poll(&mut Context::from_waker(Waker::noop()))
}

/// Lets every task run and up to a minute of virtual time pass, which is
/// longer than any single timeout of the manager.
pub async fn settle() {
    tokio::time::sleep(Duration::from_secs(60)).await;
}

/// Yields until `condition` holds; fails instead of hanging.
pub async fn wait_until(mut condition: impl FnMut() -> bool) {
    for _ in 0..1000 {
        if condition() {
            return;
        }
        tokio::task::yield_now().await;
    }
    panic!("condition not reached");
}

pub fn count(counter: &std::sync::atomic::AtomicUsize) -> usize {
    Probe::count(counter)
}

pub fn confirmed(affected: &[&str]) -> Confirmation {
    Confirmation {
        confirmed: true,
        acknowledged: affected.iter().map(|module| id(module)).collect(),
    }
}

/// Short form of a state for assertions.
pub fn kind(state: &ModuleState) -> String {
    match state {
        ModuleState::Stopped => "Stopped".into(),
        ModuleState::Initializing => "Initializing".into(),
        ModuleState::Starting => "Starting".into(),
        ModuleState::Running => "Running".into(),
        ModuleState::Stopping => "Stopping".into(),
        ModuleState::Failed { phase, .. } => format!("Failed({phase:?})"),
        ModuleState::Blocked { by, reason } => format!("Blocked({by}, {reason:?})"),
        ModuleState::ConfigUnavailable { .. } => "ConfigUnavailable".into(),
    }
}

/// A status change as (module, enabled, short state).
pub type Change = (String, Option<bool>, String);

pub fn change(module: &str, enabled: bool, state: &str) -> Change {
    (module.to_owned(), Some(enabled), state.to_owned())
}

/// A change to a status with an unknown decision (`ConfigUnavailable`).
pub fn unknown(module: &str, state: &str) -> Change {
    (module.to_owned(), None, state.to_owned())
}

pub fn enable_cycle(module: &str) -> Vec<Change> {
    vec![
        change(module, true, "Stopped"),
        change(module, true, "Initializing"),
        change(module, true, "Starting"),
        change(module, true, "Running"),
    ]
}

pub fn start_cycle(module: &str) -> Vec<Change> {
    enable_cycle(module).split_off(1)
}

pub fn stop_cycle(module: &str, enabled: bool) -> Vec<Change> {
    vec![
        change(module, enabled, "Stopping"),
        change(module, enabled, "Stopped"),
    ]
}

pub fn failed_error(status: &ModuleStatus) -> &str {
    match &status.state {
        ModuleState::Failed { error, .. } => error,
        other => panic!("expected Failed, got {other:?}"),
    }
}

pub struct Harness {
    pub manager: ModuleManagerHandle,
    pub bus: EventBus,
    pub events: Subscriber,
    pub probes: BTreeMap<String, Arc<Probe>>,
    pub store: TestStore,
    /// Last status of each module seen in the events.
    pub seen: BTreeMap<ModuleId, ModuleStatus>,
}

/// Starts the manager for fake modules `(id, dependencies)` on the current
/// runtime with an empty store, waits until it is ready and takes the events
/// of the preparation (one `StatusChanged` per module and `ManagerReady`).
pub async fn harness(modules: &[(&str, &[&str])]) -> Harness {
    let mut h = Harness::spawn(modules, &[], TestStore::default());
    h.ready().await;
    h.events();
    h
}

impl Harness {
    /// Creates the manager for fake modules `(id, dependencies)`, those in
    /// `enabled_by_default` with `default_enabled = true`, on `store`, and
    /// spawns its actor. Returns at once: the actor runs when the test
    /// awaits, so probes and the store can still be scripted.
    pub fn spawn(
        modules: &[(&str, &[&str])],
        enabled_by_default: &[&str],
        store: TestStore,
    ) -> Self {
        let bus = EventBus::new(4096);
        let events = bus.subscribe();
        let mut catalog: Vec<Box<dyn ModuleFactory>> = Vec::new();
        let mut probes = BTreeMap::new();
        let mut seen = BTreeMap::new();
        for (module, dependencies) in modules {
            let default = enabled_by_default.contains(module);
            let (factory, probe) = FakeFactory::with_default(module, dependencies, default);
            catalog.push(Box::new(factory));
            probes.insert((*module).to_owned(), probe);
            let before_loading = ModuleStatus {
                enabled: None,
                state: ModuleState::Stopped,
            };
            seen.insert(id(module), before_loading);
        }
        let (manager, actor) = manager::create(catalog, Box::new(store.clone()), bus.clone());
        tokio::spawn(actor);
        Self {
            manager,
            bus,
            events,
            probes,
            store,
            seen,
        }
    }

    /// Waits (in virtual time) until the manager is ready; fails instead of
    /// hanging.
    pub async fn ready(&self) {
        for _ in 0..100_000 {
            if self.manager.is_ready() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("the manager did not become ready");
    }

    pub fn probe(&self, module: &str) -> &Arc<Probe> {
        &self.probes[module]
    }

    pub fn status(&self, module: &str) -> ModuleStatus {
        self.manager.statuses().expect("ready")[&id(module)].clone()
    }

    pub async fn apply(
        &self,
        action: ChangeAction,
        target: &str,
        confirmation: Confirmation,
    ) -> Result<ApplyOutcome, ManagerError> {
        self.manager.apply(action, id(target), confirmation).await
    }

    /// Applies `action`, confirming exactly the affected modules of the
    /// current plan, and expects it to be applied.
    pub async fn confirm(&self, action: ChangeAction, target: &str) -> ApplyReport {
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
    pub fn events(&mut self) -> Vec<ModuleEvent> {
        let mut events = Vec::new();
        while let Poll::Ready(received) = poll_once(self.events.recv()) {
            let Received::Event(envelope) = received else {
                panic!("unexpected {received:?}");
            };
            let AppEvent::Module(event) = envelope.event;
            if event == ModuleEvent::ManagerReady {
                assert!(
                    self.manager.is_ready(),
                    "`ManagerReady` comes after the snapshot is ready"
                );
            }
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
        if let Ok(snapshot) = self.manager.statuses() {
            assert!(snapshot.values().all(ModuleStatus::is_consistent));
            assert!(
                snapshot.values().all(|status| status.enabled.is_some()
                    || matches!(status.state, ModuleState::ConfigUnavailable { .. })),
                "after ready an unknown decision goes only with ConfigUnavailable"
            );
            assert_eq!(
                snapshot, self.seen,
                "the snapshot matches the published changes"
            );
        }
        events
    }

    /// Status changes since the last call, in short form.
    pub fn changes(&mut self) -> Vec<Change> {
        self.events()
            .into_iter()
            .filter_map(|event| match event {
                ModuleEvent::StatusChanged { id, current, .. } => {
                    Some((id.to_string(), current.enabled, kind(&current.state)))
                }
                ModuleEvent::HealthChanged { .. }
                | ModuleEvent::SettingsChanged { .. }
                | ModuleEvent::ManagerReady => None,
            })
            .collect()
    }

    /// Health results published since the last call.
    pub fn health_events(&mut self) -> Vec<(String, Health)> {
        self.events()
            .into_iter()
            .filter_map(|event| match event {
                ModuleEvent::HealthChanged { id, health } => Some((id.to_string(), health)),
                _ => None,
            })
            .collect()
    }

    /// Settings changes published since the last call.
    pub fn settings_events(&mut self) -> Vec<(String, bool)> {
        self.events()
            .into_iter()
            .filter_map(|event| match event {
                ModuleEvent::SettingsChanged {
                    id,
                    restart_required,
                } => Some((id.to_string(), restart_required)),
                _ => None,
            })
            .collect()
    }
}

thread_local! {
    /// Log output of the current test thread while it captures logs.
    static CAPTURED: RefCell<Option<Arc<Mutex<Vec<u8>>>>> = const { RefCell::new(None) };
}

/// Writes to the log buffer of the current thread, if it captures logs.
pub struct ThreadLog;

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
pub fn capture_logs(test: impl FnOnce()) -> String {
    static INSTALL: Once = Once::new();
    INSTALL.call_once(|| {
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::DEBUG)
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

/// The single log line containing `message`; fails with the whole log
/// otherwise.
pub fn log_line<'a>(logs: &'a str, message: &str) -> &'a str {
    let mut lines = logs.lines().filter(|line| line.contains(message));
    let line = lines
        .next()
        .unwrap_or_else(|| panic!("no `{message}` in the log:\n{logs}"));
    assert!(lines.next().is_none(), "`{message}` logged twice:\n{logs}");
    line
}

/// Like `wait_until`, for a condition that is reached only after an instance
/// drop on the blocking pool (D1): between yields it gives the pool threads
/// real time, which matters on a single core. Fails after ten seconds.
pub async fn wait_until_with_pool(mut condition: impl FnMut() -> bool) {
    let limit = std::time::Instant::now() + Duration::from_secs(10);
    while !condition() {
        assert!(std::time::Instant::now() < limit, "condition not reached");
        tokio::task::yield_now().await;
        std::thread::sleep(Duration::from_millis(1));
    }
}
