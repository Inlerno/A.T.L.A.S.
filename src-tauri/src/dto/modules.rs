//! DTOs of the module domain (issue #11): modules, change plans, results of
//! applying a plan, settings and the events forwarded to the webview.
//!
//! Mirrored in `src/types/modules.ts`; the test at the end of this file fails
//! when a key or a tag here is missing there.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::events::types::{AppEvent, EventEnvelope, ModuleEvent};
use crate::modules::contract::{
    BlockReason, FailurePhase, Health, ModuleDescriptor, ModuleId, ModuleState, ModuleStatus,
};
use crate::modules::manager::{
    ApplyOutcome, ApplyReport, FailedModule, ModuleSettings, ModuleView,
};
use crate::modules::plan::{Blocker, ChangeAction, ChangePlan, PlanStep};

fn ids(ids: Vec<ModuleId>) -> Vec<String> {
    ids.into_iter().map(|id| id.to_string()).collect()
}

/// A module of the catalog with its current status.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModuleDto {
    pub descriptor: ModuleDescriptorDto,
    /// The user's decision; `null` — unknown: the configuration record could
    /// not be read (`configUnavailable`).
    pub enabled: Option<bool>,
    pub state: ModuleStateDto,
    /// Last health result of the running instance; `null` until it is
    /// checked. Informational only.
    pub health: Option<HealthDto>,
    /// The running instance was created with other settings; they apply only
    /// after a confirmed `restart`.
    pub restart_required: bool,
}

impl From<ModuleView> for ModuleDto {
    fn from(view: ModuleView) -> Self {
        Self {
            descriptor: view.descriptor.into(),
            enabled: view.status.enabled,
            state: view.status.state.into(),
            health: view.health.map(Into::into),
            restart_required: view.restart_required,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModuleDescriptorDto {
    pub id: String,
    pub display_name: String,
    pub description: String,
    pub version: String,
    pub dependencies: Vec<String>,
    pub default_enabled: bool,
}

impl From<ModuleDescriptor> for ModuleDescriptorDto {
    fn from(descriptor: ModuleDescriptor) -> Self {
        Self {
            id: descriptor.id.to_string(),
            display_name: descriptor.display_name,
            description: descriptor.description,
            version: descriptor.version,
            dependencies: ids(descriptor.dependencies),
            default_enabled: descriptor.default_enabled,
        }
    }
}

/// The user's decision and the actual state, as events carry them.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModuleStatusDto {
    pub enabled: Option<bool>,
    pub state: ModuleStateDto,
}

impl From<ModuleStatus> for ModuleStatusDto {
    fn from(status: ModuleStatus) -> Self {
        Self {
            enabled: status.enabled,
            state: status.state.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum ModuleStateDto {
    Stopped,
    Initializing,
    Starting,
    Running,
    Stopping,
    Failed {
        phase: FailurePhaseDto,
        /// User-safe text.
        error: String,
        /// Unix time in milliseconds.
        at: u64,
    },
    Blocked {
        /// The root-cause module.
        by: String,
        reason: BlockReasonDto,
    },
    ConfigUnavailable {
        /// User-safe text.
        error: String,
        /// Unix time in milliseconds.
        at: u64,
    },
}

impl From<ModuleState> for ModuleStateDto {
    fn from(state: ModuleState) -> Self {
        match state {
            ModuleState::Stopped => Self::Stopped,
            ModuleState::Initializing => Self::Initializing,
            ModuleState::Starting => Self::Starting,
            ModuleState::Running => Self::Running,
            ModuleState::Stopping => Self::Stopping,
            ModuleState::Failed { phase, error, at } => Self::Failed {
                phase: phase.into(),
                error,
                at,
            },
            ModuleState::Blocked { by, reason } => Self::Blocked {
                by: by.to_string(),
                reason: reason.into(),
            },
            ModuleState::ConfigUnavailable { error, at } => Self::ConfigUnavailable { error, at },
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum FailurePhaseDto {
    Initialize,
    Start,
    Run,
    Stop,
    HealthCheck,
    Settings,
}

impl From<FailurePhase> for FailurePhaseDto {
    fn from(phase: FailurePhase) -> Self {
        match phase {
            FailurePhase::Initialize => Self::Initialize,
            FailurePhase::Start => Self::Start,
            FailurePhase::Run => Self::Run,
            FailurePhase::Stop => Self::Stop,
            FailurePhase::HealthCheck => Self::HealthCheck,
            FailurePhase::Settings => Self::Settings,
        }
    }
}

// As in the contract: every reason is about a dependency.
#[allow(clippy::enum_variant_names)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum BlockReasonDto {
    DependencyFailed,
    DependencyDisabled,
    DependencyConfigUnavailable,
}

impl From<BlockReason> for BlockReasonDto {
    fn from(reason: BlockReason) -> Self {
        match reason {
            BlockReason::DependencyFailed => Self::DependencyFailed,
            BlockReason::DependencyDisabled => Self::DependencyDisabled,
            BlockReason::DependencyConfigUnavailable => Self::DependencyConfigUnavailable,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "camelCase")]
pub enum HealthDto {
    Healthy,
    Degraded { message: String },
    Unhealthy { message: String },
}

impl From<Health> for HealthDto {
    fn from(health: Health) -> Self {
        match health {
            Health::Healthy => Self::Healthy,
            Health::Degraded(message) => Self::Degraded { message },
            Health::Unhealthy(message) => Self::Unhealthy { message },
        }
    }
}

/// The action of a change, as the UI names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ChangeActionDto {
    Enable,
    Disable,
    Recover,
    Restart,
}

impl From<ChangeActionDto> for ChangeAction {
    fn from(action: ChangeActionDto) -> Self {
        match action {
            ChangeActionDto::Enable => Self::Enable,
            ChangeActionDto::Disable => Self::Disable,
            ChangeActionDto::Recover => Self::Recover,
            ChangeActionDto::Restart => Self::Restart,
        }
    }
}

impl From<ChangeAction> for ChangeActionDto {
    fn from(action: ChangeAction) -> Self {
        match action {
            ChangeAction::Enable => Self::Enable,
            ChangeAction::Disable => Self::Disable,
            ChangeAction::Recover => Self::Recover,
            ChangeAction::Restart => Self::Restart,
        }
    }
}

/// A change plan to show before confirmation.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ChangePlanDto {
    pub action: ChangeActionDto,
    pub target: String,
    /// Modules affected besides the target: the list the user confirms and
    /// sends back as `acknowledged`.
    pub affected: Vec<String>,
    pub steps: Vec<PlanStepDto>,
    pub requires_confirmation: bool,
    /// Not empty when the plan cannot be applied.
    pub blockers: Vec<BlockerDto>,
}

impl From<ChangePlan> for ChangePlanDto {
    fn from(plan: ChangePlan) -> Self {
        Self {
            action: plan.action.into(),
            target: plan.target.to_string(),
            affected: ids(plan.affected),
            steps: plan.steps.into_iter().map(Into::into).collect(),
            requires_confirmation: plan.requires_confirmation,
            blockers: plan.blockers.into_iter().map(Into::into).collect(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum PlanStepDto {
    Enable { id: String },
    Start { id: String },
    Stop { id: String },
    Disable { id: String },
}

impl From<PlanStep> for PlanStepDto {
    fn from(step: PlanStep) -> Self {
        match step {
            PlanStep::Enable(id) => Self::Enable { id: id.to_string() },
            PlanStep::Start(id) => Self::Start { id: id.to_string() },
            PlanStep::Stop(id) => Self::Stop { id: id.to_string() },
            PlanStep::Disable(id) => Self::Disable { id: id.to_string() },
        }
    }
}

/// A module that prevents a plan from being applied.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BlockerDto {
    pub id: String,
    pub state: ModuleStateDto,
}

impl From<Blocker> for BlockerDto {
    fn from(blocker: Blocker) -> Self {
        Self {
            id: blocker.id.to_string(),
            state: blocker.state.into(),
        }
    }
}

/// Result of `apply_module_change`. Every variant is an expected outcome, not
/// an error; only `applied` changed anything.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "outcome", rename_all = "camelCase")]
pub enum ApplyResultDto {
    Applied {
        report: ApplyReportDto,
    },
    /// The plan needs the user's confirmation (`recover`: always).
    ConfirmationRequired {
        plan: ChangePlanDto,
    },
    /// The confirmed list differs from the current one: show the new plan.
    ImpactChanged {
        plan: ChangePlanDto,
    },
    /// Other modules must be recovered first.
    NotApplicable {
        blockers: Vec<BlockerDto>,
    },
}

impl From<ApplyOutcome> for ApplyResultDto {
    fn from(outcome: ApplyOutcome) -> Self {
        match outcome {
            ApplyOutcome::Applied(report) => Self::Applied {
                report: report.into(),
            },
            ApplyOutcome::ConfirmationRequired(plan) => {
                Self::ConfirmationRequired { plan: plan.into() }
            }
            ApplyOutcome::ImpactChanged(plan) => Self::ImpactChanged { plan: plan.into() },
            ApplyOutcome::NotApplicable(plan) => Self::NotApplicable {
                blockers: plan.blockers.into_iter().map(Into::into).collect(),
            },
        }
    }
}

/// What an applied plan did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ApplyReportDto {
    pub started: Vec<String>,
    pub stopped: Vec<String>,
    /// Failed while the plan ran; not retried.
    pub failed: Vec<FailedModuleDto>,
    /// Not started because a dependency failed while the plan ran.
    pub blocked: Vec<String>,
    /// The application started to exit; the remaining steps were skipped.
    pub interrupted: bool,
}

impl From<ApplyReport> for ApplyReportDto {
    fn from(report: ApplyReport) -> Self {
        Self {
            started: ids(report.started),
            stopped: ids(report.stopped),
            failed: report.failed.into_iter().map(Into::into).collect(),
            blocked: ids(report.blocked),
            interrupted: report.interrupted,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FailedModuleDto {
    pub id: String,
    /// User-safe text.
    pub error: String,
}

impl From<FailedModule> for FailedModuleDto {
    fn from(failed: FailedModule) -> Self {
        Self {
            id: failed.id.to_string(),
            error: failed.error,
        }
    }
}

/// Settings of a module. The value goes only to the webview that asked for
/// it: never to events or the log.
#[derive(Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModuleSettingsDto {
    pub id: String,
    pub value: Value,
    /// `false`: the stored value failed the checks at startup.
    pub valid: bool,
    pub restart_required: bool,
}

/// Without the settings value.
impl std::fmt::Debug for ModuleSettingsDto {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ModuleSettingsDto")
            .field("id", &self.id)
            .field("value", &"<hidden>")
            .field("valid", &self.valid)
            .field("restart_required", &self.restart_required)
            .finish()
    }
}

impl ModuleSettingsDto {
    pub fn new(id: &ModuleId, settings: ModuleSettings) -> Self {
        Self {
            id: id.to_string(),
            value: Value::clone(&settings.value),
            valid: settings.valid,
            restart_required: settings.restart_required,
        }
    }
}

/// An event of the bus as the webview receives it (`modules://event`).
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UiEventDto {
    /// Publication number on the bus: strictly increasing, but not by one —
    /// the bus also carries events that are not sent to the UI. Missed events
    /// are reported by `modules://resync`, not by a gap.
    pub seq: u64,
    pub at_unix_ms: u64,
    pub event: ModuleEventDto,
}

impl UiEventDto {
    /// `None` for an event the UI is not meant to see. Every lifecycle event
    /// is forwarded; the exhaustive match makes a new variant an explicit
    /// decision.
    pub fn from_envelope(envelope: EventEnvelope) -> Option<Self> {
        let event = match envelope.event {
            AppEvent::Module(ModuleEvent::StatusChanged {
                id,
                previous,
                current,
            }) => ModuleEventDto::StatusChanged {
                id: id.to_string(),
                previous: previous.into(),
                current: current.into(),
            },
            AppEvent::Module(ModuleEvent::HealthChanged { id, health }) => {
                ModuleEventDto::HealthChanged {
                    id: id.to_string(),
                    health: health.into(),
                }
            }
            AppEvent::Module(ModuleEvent::SettingsChanged {
                id,
                restart_required,
            }) => ModuleEventDto::SettingsChanged {
                id: id.to_string(),
                restart_required,
            },
            AppEvent::Module(ModuleEvent::ManagerReady) => ModuleEventDto::ManagerReady,
            // Demo events stay inside the core: the UI has no use for them.
            #[cfg(feature = "demo-modules")]
            AppEvent::Demo(_) => return None,
        };
        Some(Self {
            seq: envelope.seq,
            at_unix_ms: envelope.at_unix_ms,
            event,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum ModuleEventDto {
    StatusChanged {
        id: String,
        previous: ModuleStatusDto,
        current: ModuleStatusDto,
    },
    HealthChanged {
        id: String,
        health: HealthDto,
    },
    /// Carries no settings values.
    SettingsChanged {
        id: String,
        restart_required: bool,
    },
    ManagerReady,
}

/// Payload of `modules://resync`: events were missed, load the list again.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ResyncDto {
    pub missed: u64,
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::sync::Arc;

    use serde_json::json;

    use super::*;
    use crate::dto::ErrorDto;
    use crate::events::types::EventSource;
    use crate::modules::manager::ManagerError;

    /// The hand-written mirror of these DTOs.
    const TS_TYPES: &str = include_str!("../../../src/types/modules.ts");

    fn id(value: &str) -> ModuleId {
        ModuleId::new(value).expect("valid module id")
    }

    fn failed() -> ModuleState {
        ModuleState::Failed {
            phase: FailurePhase::HealthCheck,
            error: "x".into(),
            at: 1,
        }
    }

    fn all_states() -> Vec<ModuleState> {
        let mut states = vec![
            ModuleState::Stopped,
            ModuleState::Initializing,
            ModuleState::Starting,
            ModuleState::Running,
            ModuleState::Stopping,
            ModuleState::ConfigUnavailable {
                error: "x".into(),
                at: 1,
            },
        ];
        for phase in [
            FailurePhase::Initialize,
            FailurePhase::Start,
            FailurePhase::Run,
            FailurePhase::Stop,
            FailurePhase::HealthCheck,
            FailurePhase::Settings,
        ] {
            states.push(ModuleState::Failed {
                phase,
                error: "x".into(),
                at: 1,
            });
        }
        for reason in [
            BlockReason::DependencyFailed,
            BlockReason::DependencyDisabled,
            BlockReason::DependencyConfigUnavailable,
        ] {
            states.push(ModuleState::Blocked {
                by: id("t.a"),
                reason,
            });
        }
        states
    }

    fn view(state: ModuleState, health: Option<Health>) -> ModuleView {
        let enabled = match state {
            ModuleState::ConfigUnavailable { .. } => None,
            _ => Some(true),
        };
        ModuleView {
            descriptor: ModuleDescriptor {
                id: id("t.b"),
                display_name: "B".into(),
                description: "d".into(),
                version: "1".into(),
                dependencies: vec![id("t.a")],
                default_enabled: true,
            },
            status: ModuleStatus { enabled, state },
            health,
            restart_required: true,
        }
    }

    fn plan(action: ChangeAction) -> ChangePlan {
        ChangePlan {
            action,
            target: id("t.b"),
            affected: vec![id("t.a")],
            steps: vec![
                PlanStep::Enable(id("t.a")),
                PlanStep::Start(id("t.a")),
                PlanStep::Stop(id("t.b")),
                PlanStep::Disable(id("t.b")),
            ],
            requires_confirmation: true,
            blockers: vec![Blocker {
                id: id("t.a"),
                state: failed(),
            }],
        }
    }

    fn envelope(event: ModuleEvent) -> EventEnvelope {
        EventEnvelope {
            seq: 7,
            at_unix_ms: 1_700_000_000_000,
            source: EventSource::Core,
            event: AppEvent::Module(event),
        }
    }

    /// One JSON sample of every DTO and of every variant of every enum.
    fn samples() -> Vec<Value> {
        let mut samples = Vec::new();
        let mut add = |value: Value| samples.push(value);
        for state in all_states() {
            add(to(&ModuleDto::from(view(state, None))));
        }
        for health in [
            Health::Healthy,
            Health::Degraded("slow".into()),
            Health::Unhealthy("down".into()),
        ] {
            add(to(&ModuleDto::from(view(
                ModuleState::Running,
                Some(health.clone()),
            ))));
            add(to(&UiEventDto::from_envelope(envelope(
                ModuleEvent::HealthChanged {
                    id: id("t.a"),
                    health,
                },
            ))
            .expect("forwarded")));
        }
        for action in [
            ChangeAction::Enable,
            ChangeAction::Disable,
            ChangeAction::Recover,
            ChangeAction::Restart,
        ] {
            add(to(&ChangePlanDto::from(plan(action))));
        }
        let report = ApplyReport {
            started: vec![id("t.a")],
            stopped: vec![id("t.b")],
            failed: vec![FailedModule {
                id: id("t.c"),
                error: "x".into(),
            }],
            blocked: vec![id("t.d")],
            interrupted: true,
        };
        for outcome in [
            ApplyOutcome::Applied(report),
            ApplyOutcome::ConfirmationRequired(plan(ChangeAction::Recover)),
            ApplyOutcome::ImpactChanged(plan(ChangeAction::Disable)),
            ApplyOutcome::NotApplicable(plan(ChangeAction::Enable)),
        ] {
            add(to(&ApplyResultDto::from(outcome)));
        }
        add(to(&ModuleSettingsDto::new(
            &id("t.a"),
            ModuleSettings {
                value: Arc::new(json!({})),
                valid: true,
                restart_required: false,
            },
        )));
        let status = ModuleStatus {
            enabled: Some(true),
            state: ModuleState::Running,
        };
        for event in [
            ModuleEvent::StatusChanged {
                id: id("t.a"),
                previous: ModuleStatus::default(),
                current: status,
            },
            ModuleEvent::SettingsChanged {
                id: id("t.a"),
                restart_required: true,
            },
            ModuleEvent::ManagerReady,
        ] {
            add(to(
                &UiEventDto::from_envelope(envelope(event)).expect("forwarded")
            ));
        }
        add(to(&ResyncDto { missed: 3 }));
        add(to(&ErrorDto::from(ManagerError::NotReady)));
        samples
    }

    fn to<T: Serialize>(value: &T) -> Value {
        serde_json::to_value(value).expect("a DTO serializes to JSON")
    }

    /// Collects every object key and every value of a tag field.
    fn collect(value: &Value, keys: &mut BTreeSet<String>, tags: &mut BTreeSet<String>) {
        match value {
            Value::Object(map) => {
                for (key, child) in map {
                    keys.insert(key.clone());
                    let is_tag = matches!(
                        key.as_str(),
                        "kind" | "status" | "outcome" | "type" | "phase" | "reason" | "action"
                    );
                    if let (true, Value::String(tag)) = (is_tag, child) {
                        tags.insert(tag.clone());
                    }
                    collect(child, keys, tags);
                }
            }
            Value::Array(items) => {
                for item in items {
                    collect(item, keys, tags);
                }
            }
            _ => {}
        }
    }

    #[test]
    fn json_keys_are_camel_case_and_match_the_typescript_types() {
        let (mut keys, mut tags) = (BTreeSet::new(), BTreeSet::new());
        for sample in samples() {
            collect(&sample, &mut keys, &mut tags);
        }
        // Keys that must be there: a missing rename would lose them.
        for key in [
            "displayName",
            "defaultEnabled",
            "restartRequired",
            "requiresConfirmation",
            "atUnixMs",
        ] {
            assert!(keys.contains(key), "{key} is serialized: {keys:?}");
        }
        for key in &keys {
            assert!(
                !key.contains('_') && key.starts_with(|c: char| c.is_ascii_lowercase()),
                "`{key}` is camelCase"
            );
            let declared =
                TS_TYPES.contains(&format!("{key}: ")) || TS_TYPES.contains(&format!("{key}?: "));
            assert!(declared, "`{key}` is declared in src/types/modules.ts");
        }
        for tag in [
            "configUnavailable",
            "dependencyConfigUnavailable",
            "settings",
            "healthCheck",
            "confirmationRequired",
            "impactChanged",
            "notApplicable",
            "managerReady",
            "settingsChanged",
        ] {
            assert!(tags.contains(tag), "{tag} is serialized: {tags:?}");
        }
        for tag in &tags {
            assert!(
                TS_TYPES.contains(&format!("\"{tag}\"")),
                "\"{tag}\" is declared in src/types/modules.ts"
            );
        }
    }

    #[test]
    fn error_codes_are_declared_in_the_typescript_types() {
        use crate::dto::code;
        for code in [
            code::INVALID_ID,
            code::UNKNOWN_MODULE,
            code::ACTION_NOT_APPLICABLE,
            code::INVALID_SETTINGS,
            code::NOT_RUNNING,
            code::NOT_READY,
            code::SHUT_DOWN,
            code::CONFIG_UNAVAILABLE,
            code::STORE,
            code::INTERNAL,
        ] {
            assert!(TS_TYPES.contains(&format!("\"{code}\"")), "{code}");
        }
    }

    #[test]
    fn a_module_serializes_in_the_documented_shape() {
        let dto = ModuleDto::from(view(
            ModuleState::Blocked {
                by: id("t.a"),
                reason: BlockReason::DependencyConfigUnavailable,
            },
            None,
        ));
        assert_eq!(
            serde_json::to_value(dto).expect("json"),
            json!({
                "descriptor": {
                    "id": "t.b",
                    "displayName": "B",
                    "description": "d",
                    "version": "1",
                    "dependencies": ["t.a"],
                    "defaultEnabled": true
                },
                "enabled": true,
                "state": { "kind": "blocked", "by": "t.a", "reason": "dependencyConfigUnavailable" },
                "health": null,
                "restartRequired": true
            })
        );
        let unavailable = ModuleDto::from(view(
            ModuleState::ConfigUnavailable {
                error: "e".into(),
                at: 5,
            },
            None,
        ));
        let json = serde_json::to_value(unavailable).expect("json");
        assert_eq!(json["enabled"], Value::Null);
        assert_eq!(
            json["state"],
            json!({ "kind": "configUnavailable", "error": "e", "at": 5 })
        );
    }

    #[test]
    fn events_serialize_in_the_documented_shape() {
        let ready = UiEventDto::from_envelope(envelope(ModuleEvent::ManagerReady)).expect("ready");
        assert_eq!(
            serde_json::to_value(ready).expect("json"),
            json!({ "seq": 7, "atUnixMs": 1_700_000_000_000_u64, "event": { "type": "managerReady" } })
        );
        let settings = UiEventDto::from_envelope(envelope(ModuleEvent::SettingsChanged {
            id: id("t.a"),
            restart_required: true,
        }))
        .expect("settings");
        assert_eq!(
            serde_json::to_value(settings).expect("json")["event"],
            json!({ "type": "settingsChanged", "id": "t.a", "restartRequired": true })
        );
    }

    #[test]
    fn actions_are_read_in_camel_case() {
        for (text, action) in [
            ("enable", ChangeAction::Enable),
            ("disable", ChangeAction::Disable),
            ("recover", ChangeAction::Recover),
            ("restart", ChangeAction::Restart),
        ] {
            let dto: ChangeActionDto = serde_json::from_value(json!(text)).expect("action");
            assert_eq!(ChangeAction::from(dto), action);
        }
        assert!(serde_json::from_value::<ChangeActionDto>(json!("destroy")).is_err());
    }
}
