//! Demo modules for manual checks of the module manager (issue #12).
//!
//! Compiled only with the feature `demo-modules`; a normal build has none of
//! this. Three modules form a chain: `demo.base` ← `demo.service` ←
//! `demo.client`. Their settings make failures happen on request, so every
//! path of the manager can be tried by hand:
//!
//! - `fail_on_start` — `start` returns an error;
//! - `panic_after_secs` — a background task panics that many seconds after
//!   the start;
//! - `tick_interval_secs` (`demo.service` only) — how often it publishes
//!   `DemoEvent::Tick` on the central bus; `demo.client` subscribes and logs
//!   each tick with its source.
//!
//! The modules do nothing else: no files, no network, no OS calls.

use std::time::Duration;

use serde_json::{Value, json};

use super::context::ModuleContext;
use super::contract::{
    BoxFuture, Health, Module, ModuleDescriptor, ModuleFactory, ModuleFailure, ModuleId,
};
use crate::events::bus::{PublishError, Received};
use crate::events::types::{AppEvent, DemoEvent};

pub const BASE: &str = "demo.base";
pub const SERVICE: &str = "demo.service";
pub const CLIENT: &str = "demo.client";

const FAIL_ON_START: &str = "fail_on_start";
const PANIC_AFTER_SECS: &str = "panic_after_secs";
const TICK_INTERVAL_SECS: &str = "tick_interval_secs";

const DEFAULT_TICK_INTERVAL_SECS: u64 = 5;
const MAX_PANIC_AFTER_SECS: u64 = 86_400;
const MAX_TICK_INTERVAL_SECS: u64 = 3_600;

/// Error of `start` when `fail_on_start` is set.
pub const START_REFUSED: &str = "DEMO: запуск отклонён настройкой fail_on_start";

/// The demo modules, in no particular order: the manager sorts them.
pub fn factories() -> Vec<Box<dyn ModuleFactory>> {
    vec![
        Box::new(DemoFactory::new(Role::Client)),
        Box::new(DemoFactory::new(Role::Service)),
        Box::new(DemoFactory::new(Role::Base)),
    ]
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Role {
    Base,
    Service,
    Client,
}

impl Role {
    fn id(self) -> &'static str {
        match self {
            Self::Base => BASE,
            Self::Service => SERVICE,
            Self::Client => CLIENT,
        }
    }
}

fn module_id(id: &str) -> ModuleId {
    ModuleId::new(id).expect("demo module ids are valid")
}

struct DemoFactory {
    role: Role,
    descriptor: ModuleDescriptor,
}

impl DemoFactory {
    fn new(role: Role) -> Self {
        let (display_name, description, dependencies) = match role {
            Role::Base => ("DEMO Base", "Демо-модуль без зависимостей.", Vec::new()),
            Role::Service => (
                "DEMO Service",
                "Демо-модуль: публикует событие Tick на общей шине. Зависит от demo.base.",
                vec![module_id(BASE)],
            ),
            Role::Client => (
                "DEMO Client",
                "Демо-модуль: получает Tick и пишет его в журнал. Зависит от demo.service.",
                vec![module_id(SERVICE)],
            ),
        };
        Self {
            role,
            descriptor: ModuleDescriptor {
                id: module_id(role.id()),
                display_name: display_name.to_owned(),
                description: description.to_owned(),
                version: env!("CARGO_PKG_VERSION").to_owned(),
                dependencies,
                default_enabled: false,
            },
        }
    }
}

impl ModuleFactory for DemoFactory {
    fn descriptor(&self) -> &ModuleDescriptor {
        &self.descriptor
    }

    fn create(&self) -> Box<dyn Module> {
        Box::new(DemoModule {
            role: self.role,
            settings: Settings::default(),
        })
    }

    fn default_settings(&self) -> Value {
        let mut settings = json!({ FAIL_ON_START: false, PANIC_AFTER_SECS: null });
        if self.role == Role::Service {
            settings[TICK_INTERVAL_SECS] = json!(DEFAULT_TICK_INTERVAL_SECS);
        }
        settings
    }

    fn validate_settings(&self, settings: &Value) -> Result<(), ModuleFailure> {
        Settings::parse(self.role, settings)
            .map(drop)
            .map_err(ModuleFailure::new)
    }
}

/// Settings of a demo module. A missing field has its default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Settings {
    fail_on_start: bool,
    panic_after: Option<Duration>,
    tick_interval: Duration,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            fail_on_start: false,
            panic_after: None,
            tick_interval: Duration::from_secs(DEFAULT_TICK_INTERVAL_SECS),
        }
    }
}

impl Settings {
    /// Reads the settings of `role`. The error names the field, never the
    /// value (ADR-015).
    fn parse(role: Role, value: &Value) -> Result<Self, String> {
        let Value::Object(fields) = value else {
            return Err("настройки должны быть объектом JSON".to_owned());
        };
        let mut settings = Self::default();
        for (key, value) in fields {
            match key.as_str() {
                FAIL_ON_START => {
                    settings.fail_on_start = value.as_bool().ok_or_else(|| {
                        format!("поле `{FAIL_ON_START}` должно быть true или false")
                    })?;
                }
                PANIC_AFTER_SECS => {
                    settings.panic_after = match value {
                        Value::Null => None,
                        other => Some(seconds(other, MAX_PANIC_AFTER_SECS).ok_or_else(|| {
                            format!(
                                "поле `{PANIC_AFTER_SECS}` должно быть null или целым числом от 1 до {MAX_PANIC_AFTER_SECS}"
                            )
                        })?),
                    };
                }
                TICK_INTERVAL_SECS if role == Role::Service => {
                    settings.tick_interval =
                        seconds(value, MAX_TICK_INTERVAL_SECS).ok_or_else(|| {
                            format!(
                                "поле `{TICK_INTERVAL_SECS}` должно быть целым числом от 1 до {MAX_TICK_INTERVAL_SECS}"
                            )
                        })?;
                }
                unknown => return Err(format!("неизвестное поле `{unknown}`")),
            }
        }
        Ok(settings)
    }
}

/// A whole number of seconds from 1 to `max`.
fn seconds(value: &Value, max: u64) -> Option<Duration> {
    let seconds = value.as_u64()?;
    (1..=max)
        .contains(&seconds)
        .then(|| Duration::from_secs(seconds))
}

struct DemoModule {
    role: Role,
    settings: Settings,
}

impl Module for DemoModule {
    fn initialize<'a>(
        &'a mut self,
        ctx: &'a ModuleContext,
    ) -> BoxFuture<'a, Result<(), ModuleFailure>> {
        Box::pin(async move {
            // Already checked by the manager; checked again, because the
            // module must not trust what it did not parse itself.
            self.settings =
                Settings::parse(self.role, ctx.settings()).map_err(ModuleFailure::new)?;
            tracing::info!(module = %ctx.id(), "demo module initialized");
            Ok(())
        })
    }

    fn start<'a>(&'a mut self, ctx: &'a ModuleContext) -> BoxFuture<'a, Result<(), ModuleFailure>> {
        Box::pin(async move {
            if self.settings.fail_on_start {
                return Err(ModuleFailure::new(START_REFUSED));
            }
            if let Some(after) = self.settings.panic_after {
                let token = ctx.cancellation_token();
                ctx.spawn("demo-panic", async move {
                    if token
                        .run_until_cancelled(tokio::time::sleep(after))
                        .await
                        .is_some()
                    {
                        panic!("demo panic requested by {PANIC_AFTER_SECS}");
                    }
                    Ok(())
                });
            }
            match self.role {
                Role::Base => {}
                Role::Service => ctx.spawn("demo-ticker", tick(ctx.clone(), self.settings)),
                Role::Client => {
                    // Subscribed before `start` returns: no tick of a service
                    // started later can be missed.
                    let events = ctx.subscribe();
                    ctx.spawn("demo-listener", listen(ctx.clone(), events));
                }
            }
            tracing::info!(module = %ctx.id(), "demo module started");
            Ok(())
        })
    }

    fn stop<'a>(&'a mut self, ctx: &'a ModuleContext) -> BoxFuture<'a, Result<(), ModuleFailure>> {
        Box::pin(async move {
            tracing::info!(module = %ctx.id(), "demo module stopped");
            Ok(())
        })
    }

    fn health_check(&self) -> BoxFuture<'_, Health> {
        Box::pin(async { Health::Healthy })
    }
}

/// `demo.service`: publishes `Tick { n }` at the tick interval until the
/// instance is stopped.
async fn tick(ctx: ModuleContext, settings: Settings) -> Result<(), ModuleFailure> {
    let token = ctx.cancellation_token();
    let mut n = 0u64;
    loop {
        let sleep = tokio::time::sleep(settings.tick_interval);
        if token.run_until_cancelled(sleep).await.is_none() {
            return Ok(());
        }
        n += 1;
        match ctx.publish(AppEvent::Demo(DemoEvent::Tick { n })) {
            Ok(seq) => tracing::debug!(module = %ctx.id(), n, seq, "demo tick published"),
            // The application is exiting.
            Err(PublishError::Closed) => return Ok(()),
            Err(error @ PublishError::NotAllowed) => {
                return Err(ModuleFailure::new(error.to_string()));
            }
        }
    }
}

/// `demo.client`: logs every tick with the source the bus stamped on it.
async fn listen(
    ctx: ModuleContext,
    mut events: crate::events::bus::Subscriber,
) -> Result<(), ModuleFailure> {
    let token = ctx.cancellation_token();
    loop {
        let Some(received) = token.run_until_cancelled(events.recv()).await else {
            return Ok(());
        };
        match received {
            Received::Event(envelope) => {
                if let AppEvent::Demo(DemoEvent::Tick { n }) = envelope.event {
                    tracing::info!(module = %ctx.id(), n, source = %envelope.source, "demo tick received");
                }
            }
            Received::Lagged { missed } => {
                tracing::warn!(module = %ctx.id(), missed, "demo listener missed events");
            }
            Received::Closed => return Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::bus::{EventBus, Subscriber};
    use crate::events::types::{EventSource, ModuleEvent};
    use crate::modules::config::{InMemoryConfigStore, ModuleConfig, ModuleConfigStore};
    use crate::modules::contract::{BlockReason, FailurePhase, ModuleState, ModuleStatus};
    use crate::modules::graph::ModuleGraph;
    use crate::modules::manager::{self, ApplyOutcome, ManagerError, ModuleManagerHandle, text};
    use crate::modules::plan::ChangeAction;
    use crate::modules::testing::{
        capture_logs, confirmed, id, kind, poll_once, run, settle, wait_until_with_pool,
    };

    struct Demo {
        manager: ModuleManagerHandle,
        bus: EventBus,
        events: Subscriber,
    }

    /// The real manager with the demo catalog and the given stored records.
    async fn demo(records: &[(&str, bool, Value)]) -> Demo {
        let store = InMemoryConfigStore::default();
        let batch: Vec<_> = records
            .iter()
            .map(|(module, enabled, settings)| {
                let config = ModuleConfig {
                    enabled: *enabled,
                    settings: settings.clone(),
                };
                (id(module), config)
            })
            .collect();
        assert!(poll_once(store.save_batch(&batch)).is_ready());
        let bus = EventBus::new(4096);
        let events = bus.subscribe();
        let (manager, actor) = manager::create(factories(), Box::new(store), bus.clone());
        tokio::spawn(actor);
        for _ in 0..1000 {
            if manager.is_ready() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(manager.is_ready());
        Demo {
            manager,
            bus,
            events,
        }
    }

    impl Demo {
        async fn apply(&self, action: ChangeAction, module: &str, acknowledged: &[&str]) {
            let outcome = self
                .manager
                .apply(action, id(module), confirmed(acknowledged))
                .await;
            assert!(
                matches!(outcome, Ok(ApplyOutcome::Applied(_))),
                "{action} {module}: {outcome:?}"
            );
        }

        fn state(&self, module: &str) -> String {
            kind(&self.manager.statuses().expect("ready")[&id(module)].state)
        }

        /// Events published since the last call: (source, event).
        fn drain(&mut self) -> Vec<(EventSource, AppEvent)> {
            let mut events = Vec::new();
            while let std::task::Poll::Ready(Received::Event(envelope)) =
                poll_once(self.events.recv())
            {
                events.push((envelope.source, envelope.event));
            }
            events
        }

        /// Modules in the order in which they reached `state`.
        fn order_of(&mut self, state: &str) -> Vec<String> {
            self.drain()
                .into_iter()
                .filter_map(|(_, event)| match event {
                    AppEvent::Module(ModuleEvent::StatusChanged { id, current, .. })
                        if kind(&current.state) == state =>
                    {
                        Some(id.to_string())
                    }
                    _ => None,
                })
                .collect()
        }
    }

    fn service_settings(extra: Value) -> Value {
        let mut settings =
            json!({ FAIL_ON_START: false, PANIC_AFTER_SECS: null, TICK_INTERVAL_SECS: 5 });
        for (key, value) in extra.as_object().expect("object") {
            settings[key] = value.clone();
        }
        settings
    }

    #[test]
    fn the_demo_catalog_is_a_valid_chain_with_valid_defaults() {
        let factories = factories();
        let descriptors: Vec<_> = factories.iter().map(|f| f.descriptor().clone()).collect();
        let graph = ModuleGraph::build(descriptors.clone()).expect("valid catalog");
        let order: Vec<_> = graph
            .start_order()
            .iter()
            .map(ToString::to_string)
            .collect();
        assert_eq!(order, [BASE, SERVICE, CLIENT]);
        for (factory, descriptor) in factories.iter().zip(&descriptors) {
            assert!(
                descriptor.display_name.starts_with("DEMO "),
                "{descriptor:?}"
            );
            assert!(!descriptor.default_enabled);
            let defaults = factory.default_settings();
            assert_eq!(crate::modules::config::check_settings(&defaults), Ok(()));
            assert_eq!(factory.validate_settings(&defaults), Ok(()));
        }
        // The built-in catalog of this build is the demo catalog.
        assert_eq!(crate::modules::catalog().len(), 3);
    }

    #[test]
    fn settings_are_checked_and_errors_name_the_field_not_the_value() {
        let service = DemoFactory::new(Role::Service);
        let base = DemoFactory::new(Role::Base);
        for valid in [
            json!({}),
            json!({ FAIL_ON_START: true }),
            json!({ PANIC_AFTER_SECS: 1 }),
            json!({ PANIC_AFTER_SECS: 86_400, TICK_INTERVAL_SECS: 3_600 }),
        ] {
            assert_eq!(service.validate_settings(&valid), Ok(()), "{valid}");
        }
        for (field, value) in [
            (FAIL_ON_START, json!({ FAIL_ON_START: "yes-7731" })),
            (PANIC_AFTER_SECS, json!({ PANIC_AFTER_SECS: 0 })),
            (PANIC_AFTER_SECS, json!({ PANIC_AFTER_SECS: 86_401 })),
            (PANIC_AFTER_SECS, json!({ PANIC_AFTER_SECS: 1.5 })),
            (TICK_INTERVAL_SECS, json!({ TICK_INTERVAL_SECS: 0 })),
            (
                TICK_INTERVAL_SECS,
                json!({ TICK_INTERVAL_SECS: "fast-7731" }),
            ),
            ("unknown_key", json!({ "unknown_key": "x-7731" })),
        ] {
            let failure = service.validate_settings(&value).expect_err("refused");
            assert!(failure.message().contains(field), "{}", failure.message());
            assert!(!failure.message().contains("7731"), "{}", failure.message());
        }
        // Only the service has a tick interval.
        let failure = base
            .validate_settings(&json!({ TICK_INTERVAL_SECS: 5 }))
            .expect_err("refused");
        assert!(failure.message().contains(TICK_INTERVAL_SECS));
        assert!(base.validate_settings(&json!([])).is_err());
    }

    /// Criteria of #12: enabling the client starts base → service → client;
    /// the client receives ticks whose source is `demo.service`; disabling
    /// the base stops client → service → base and disables the dependents.
    #[test]
    fn the_chain_starts_in_order_exchanges_ticks_and_stops_in_reverse() {
        let logs = capture_logs(|| {
            run(async {
                let mut d = demo(&[]).await;
                d.drain();
                let plan = d
                    .manager
                    .plan(ChangeAction::Enable, &id(CLIENT))
                    .expect("plan");
                assert_eq!(plan.affected, [id(BASE), id(SERVICE)]);
                assert!(plan.requires_confirmation);
                d.apply(ChangeAction::Enable, CLIENT, &[BASE, SERVICE])
                    .await;
                assert_eq!(d.order_of("Running"), [BASE, SERVICE, CLIENT]);

                tokio::time::sleep(Duration::from_secs(11)).await;
                let ticks: Vec<_> = d
                    .drain()
                    .into_iter()
                    .filter(|(_, event)| matches!(event, AppEvent::Demo(_)))
                    .collect();
                assert_eq!(
                    ticks,
                    [1, 2].map(|n| (
                        EventSource::Module(id(SERVICE)),
                        AppEvent::Demo(DemoEvent::Tick { n })
                    ))
                );

                let plan = d
                    .manager
                    .plan(ChangeAction::Disable, &id(BASE))
                    .expect("plan");
                assert_eq!(plan.affected, [id(CLIENT), id(SERVICE)]);
                d.apply(ChangeAction::Disable, BASE, &[CLIENT, SERVICE])
                    .await;
                assert_eq!(d.order_of("Stopping"), [CLIENT, SERVICE, BASE]);
                for module in [BASE, SERVICE, CLIENT] {
                    let status = d.manager.statuses().expect("ready")[&id(module)].clone();
                    assert_eq!(status, ModuleStatus::default(), "{module} is disabled");
                }
            });
        });
        for n in [1, 2] {
            let line = format!("demo tick received module=demo.client n={n} source=demo.service");
            assert!(logs.contains(&line), "`{line}` not in the log:\n{logs}");
        }
    }

    /// Criterion of #12: `panic_after_secs` of the service fails it with a
    /// safe text, blocks the client, leaves the base running, and nothing
    /// restarts on its own in two minutes. Then the manual `recover`.
    #[test]
    fn a_panic_fails_the_service_blocks_the_client_and_nothing_restarts_on_its_own() {
        let logs = capture_logs(|| {
            run(async {
                let settings = service_settings(json!({ PANIC_AFTER_SECS: 3 }));
                let mut d = demo(&[(SERVICE, false, settings)]).await;
                d.apply(ChangeAction::Enable, CLIENT, &[BASE, SERVICE])
                    .await;
                d.drain();
                tokio::time::sleep(Duration::from_secs(4)).await;
                let manager = d.manager.clone();
                wait_until_with_pool(move || {
                    let statuses = manager.statuses().expect("ready");
                    matches!(statuses[&id(CLIENT)].state, ModuleState::Blocked { .. })
                })
                .await;

                let statuses = d.manager.statuses().expect("ready");
                let ModuleState::Failed { phase, error, .. } = &statuses[&id(SERVICE)].state else {
                    panic!("demo.service must be failed: {statuses:?}");
                };
                assert_eq!(*phase, FailurePhase::Run);
                assert_eq!(error, text::PANICKED, "a safe text, not the panic message");
                assert_eq!(statuses[&id(SERVICE)].enabled, Some(true));
                assert_eq!(
                    statuses[&id(CLIENT)].state,
                    ModuleState::Blocked {
                        by: id(SERVICE),
                        reason: BlockReason::DependencyFailed
                    }
                );
                assert_eq!(statuses[&id(BASE)].state, ModuleState::Running);
                d.drain();

                // Two minutes: nothing changes by itself.
                tokio::time::sleep(Duration::from_secs(120)).await;
                settle().await;
                assert!(d.drain().is_empty(), "no event, no restart");
                assert_eq!(d.state(SERVICE), "Failed(Run)");

                // The client cannot be recovered while the service is failed.
                let outcome = d
                    .manager
                    .apply(ChangeAction::Recover, id(CLIENT), confirmed(&[]))
                    .await;
                let Ok(ApplyOutcome::NotApplicable(plan)) = outcome else {
                    panic!("expected not applicable: {outcome:?}");
                };
                assert_eq!(plan.blockers[0].id, id(SERVICE));

                // Recover of the service is confirmed and brings the client back.
                let plan = d
                    .manager
                    .plan(ChangeAction::Recover, &id(SERVICE))
                    .expect("plan");
                assert_eq!(plan.affected, [id(CLIENT)]);
                d.apply(ChangeAction::Recover, SERVICE, &[CLIENT]).await;
                assert_eq!(d.state(SERVICE), "Running");
                assert_eq!(d.state(CLIENT), "Running");
            });
        });
        assert!(
            logs.contains("demo panic requested by panic_after_secs"),
            "{logs}"
        );
    }

    /// Criterion of #12: `fail_on_start` fails the module with a clear error;
    /// recover of a single module without dependents still needs the
    /// confirmation.
    #[test]
    fn fail_on_start_fails_the_module_with_a_clear_error() {
        run(async {
            let settings = json!({ FAIL_ON_START: true, PANIC_AFTER_SECS: null });
            let d = demo(&[(BASE, false, settings)]).await;
            let outcome = d
                .manager
                .apply(ChangeAction::Enable, id(BASE), confirmed(&[]))
                .await;
            let Ok(ApplyOutcome::Applied(report)) = outcome else {
                panic!("applied: {outcome:?}");
            };
            assert_eq!(report.failed[0].error, START_REFUSED);
            let statuses = d.manager.statuses().expect("ready");
            let ModuleState::Failed { phase, error, .. } = &statuses[&id(BASE)].state else {
                panic!("failed: {statuses:?}");
            };
            assert_eq!(
                (*phase, error.as_str()),
                (FailurePhase::Start, START_REFUSED)
            );

            let plan = d
                .manager
                .plan(ChangeAction::Recover, &id(BASE))
                .expect("plan");
            assert!(plan.affected.is_empty() && plan.requires_confirmation);
            let unconfirmed = d
                .manager
                .apply(
                    ChangeAction::Recover,
                    id(BASE),
                    crate::modules::plan::Confirmation::default(),
                )
                .await;
            assert!(matches!(
                unconfirmed,
                Ok(ApplyOutcome::ConfirmationRequired(_))
            ));
        });
    }

    /// Criterion of #12: new settings mark a running module and apply only
    /// after a confirmed restart; invalid settings change nothing.
    #[test]
    fn new_settings_need_a_restart_and_invalid_ones_change_nothing() {
        run(async {
            let mut d = demo(&[]).await;
            d.apply(ChangeAction::Enable, SERVICE, &[BASE]).await;
            let refused = d
                .manager
                .update_settings(id(SERVICE), json!({ TICK_INTERVAL_SECS: 0 }))
                .await;
            assert!(matches!(
                refused,
                Err(ManagerError::Module(
                    crate::modules::contract::ModuleError::InvalidSettings { .. }
                ))
            ));
            let before = d.manager.get_settings(&id(SERVICE)).expect("settings");
            assert!(!before.restart_required);

            d.manager
                .update_settings(
                    id(SERVICE),
                    service_settings(json!({ TICK_INTERVAL_SECS: 1 })),
                )
                .await
                .expect("update");
            assert!(
                d.manager
                    .get_settings(&id(SERVICE))
                    .expect("settings")
                    .restart_required
            );
            d.drain();
            // Still the old interval of 5 s: one tick in 6 s.
            tokio::time::sleep(Duration::from_secs(6)).await;
            let ticks = |d: &mut Demo| {
                d.drain()
                    .into_iter()
                    .filter(|(_, event)| matches!(event, AppEvent::Demo(_)))
                    .count()
            };
            assert_eq!(ticks(&mut d), 1, "not restarted on its own");

            d.apply(ChangeAction::Restart, SERVICE, &[]).await;
            d.drain();
            tokio::time::sleep(Duration::from_millis(3500)).await;
            assert_eq!(ticks(&mut d), 3, "the new interval of 1 s");
            assert!(
                !d.manager
                    .get_settings(&id(SERVICE))
                    .expect("settings")
                    .restart_required
            );
        });
    }

    /// Criterion of #12: a module cannot publish with another source or a
    /// lifecycle event.
    #[test]
    fn a_module_publishes_only_its_own_non_lifecycle_events() {
        let bus = EventBus::new(16);
        let mut events = bus.subscribe();
        let module = id(SERVICE);
        let lifecycle = AppEvent::Module(ModuleEvent::ManagerReady);
        assert_eq!(
            bus.publish_from_module(&module, lifecycle),
            Err(PublishError::NotAllowed)
        );
        let forged = AppEvent::Module(ModuleEvent::StatusChanged {
            id: id(BASE),
            previous: ModuleStatus::default(),
            current: ModuleStatus::default(),
        });
        assert_eq!(
            bus.publish_from_module(&module, forged),
            Err(PublishError::NotAllowed)
        );
        // A refused event uses no `seq` and reaches nobody.
        let tick = AppEvent::Demo(DemoEvent::Tick { n: 1 });
        assert_eq!(bus.publish_from_module(&module, tick.clone()), Ok(1));
        let std::task::Poll::Ready(Received::Event(envelope)) = poll_once(events.recv()) else {
            panic!("the tick is delivered");
        };
        assert_eq!(envelope.seq, 1);
        assert_eq!(envelope.event, tick);
        // The source is the module the bus was told by the context: the
        // publishing API takes no source at all.
        assert_eq!(envelope.source, EventSource::Module(module));
        assert_eq!(envelope.source.to_string(), SERVICE);

        bus.close();
        assert_eq!(
            bus.publish_from_module(&id(SERVICE), AppEvent::Demo(DemoEvent::Tick { n: 2 })),
            Err(PublishError::Closed)
        );
    }

    /// Criterion of #12: closing the application with running modules stops
    /// them in reverse order without hanging.
    #[test]
    fn the_exit_stops_running_demo_modules_in_reverse_order() {
        run(async {
            let mut d = demo(&[]).await;
            d.apply(ChangeAction::Enable, CLIENT, &[BASE, SERVICE])
                .await;
            d.drain();
            let begun = tokio::time::Instant::now();
            let report = d.manager.shutdown().await.expect("exit");
            assert_eq!(report.stopped, [id(CLIENT), id(SERVICE), id(BASE)]);
            assert!(report.failed.is_empty());
            assert!(begun.elapsed() < Duration::from_secs(1));
            assert_eq!(d.order_of("Stopping"), [CLIENT, SERVICE, BASE]);
            d.bus.close();
        });
    }

    #[test]
    fn health_of_a_running_demo_module_is_healthy_and_changes_nothing() {
        run(async {
            let mut d = demo(&[]).await;
            d.apply(ChangeAction::Enable, BASE, &[]).await;
            d.drain();
            assert_eq!(d.manager.check_health(id(BASE)).await, Ok(Health::Healthy));
            assert_eq!(d.state(BASE), "Running");
            let events = d.drain();
            assert!(
                events.iter().all(|(_, event)| !matches!(
                    event,
                    AppEvent::Module(ModuleEvent::StatusChanged { .. })
                )),
                "{events:?}"
            );
        });
    }
}
