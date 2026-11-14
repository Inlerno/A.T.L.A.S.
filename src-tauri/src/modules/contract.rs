//! Module contract (issues #7, #9 and #10): identifiers, descriptors, statuses,
//! errors, and the `Module` / `ModuleFactory` traits implemented by modules.
//!
//! Status changes are published through the central typed event bus (#8)
//! using these types; only the lifecycle manager changes statuses.

use std::fmt;
use std::future::Future;
use std::pin::Pin;

use serde_json::Value;
use thiserror::Error;

use super::context::ModuleContext;
use super::plan::ChangeAction;

/// Future returned by module methods. Boxed by hand instead of using the
/// `async-trait` crate: no extra dependency, and `Box<dyn Module>` works.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// A module: compile-time Rust code that the lifecycle manager runs (#9,
/// ADR-014).
///
/// The manager creates a new instance with its factory for every start and
/// drops it after the stop or a failure. Each call runs in its own task with a
/// timeout, so a panic or a hang in a module does not stop the application.
///
/// **Resources.** The instance owns everything it allocates (files, sockets,
/// devices, native handles, threads, child processes), and dropping the
/// instance must release all of it. After a failure the manager only drops
/// the instance, and after a panic or a timeout `Drop` is the only module
/// code that still runs. Background tasks started with
/// [`ModuleContext::spawn`] are cancelled and aborted by the manager;
/// everything else is the module's responsibility.
///
/// **Dropping.** The manager drops an instance on a thread of tokio's
/// blocking pool, never in its own task, and waits for the drop only within
/// the stop limit: what is left of it after `stop`, or the whole limit after a
/// failed `initialize` or `start`. After a successful `stop`, a `Drop` that
/// panics or does not return in time fails the stop (`Failed { phase: Stop }`).
/// A `Drop` that does not return is no longer waited for but keeps running: a
/// blocking task cannot be interrupted. `Drop` should release its resources
/// promptly and never block indefinitely.
///
/// **Blocking.** A timeout can interrupt a call only where it yields
/// (`.await`). Code that blocks a thread without yielding, such as a blocking
/// native call, cannot be interrupted by an ordinary async timeout: it holds
/// the thread until it returns. Such code belongs on a separate thread (for
/// example `tokio::task::spawn_blocking`), so that the module's async methods
/// keep yielding.
pub trait Module: Send + Sync {
    /// Prepares the instance. Background work belongs in
    /// [`ModuleContext::spawn`].
    ///
    /// If it fails, the module first releases whatever it has allocated so
    /// far and only then returns the error: `stop` is not called for an
    /// instance that did not start, and dropping the instance must leave
    /// nothing behind.
    fn initialize<'a>(
        &'a mut self,
        ctx: &'a ModuleContext,
    ) -> BoxFuture<'a, Result<(), ModuleFailure>>;

    /// Starts the work after a successful `initialize`. If it fails, the same
    /// rule applies: release what was allocated, then return the error;
    /// `stop` is not called.
    fn start<'a>(&'a mut self, ctx: &'a ModuleContext) -> BoxFuture<'a, Result<(), ModuleFailure>>;

    /// Stops the work of a started instance. Called once, after the context's
    /// cancellation token is cancelled; not called if `initialize` or `start`
    /// failed.
    fn stop<'a>(&'a mut self, ctx: &'a ModuleContext) -> BoxFuture<'a, Result<(), ModuleFailure>>;

    /// Informational check on request; it never changes the module status.
    fn health_check(&self) -> BoxFuture<'_, Health>;
}

/// Catalog entry: describes a module, checks its settings and creates its
/// instances.
///
/// The settings methods belong to the factory, not to an instance: settings
/// are checked also for a module that is disabled and has no instance (#10,
/// ADR-015). All three methods must be pure and fast — no I/O, no blocking —
/// because the manager calls them on its own task. A panic in any of them is
/// caught and does not stop the manager.
pub trait ModuleFactory: Send + Sync {
    fn descriptor(&self) -> &ModuleDescriptor;

    /// Creates a new instance. State of a previous instance is never reused.
    /// The instance gets its settings through [`ModuleContext::settings`],
    /// available from `initialize` on.
    fn create(&self) -> Box<dyn Module>;

    /// Settings of a module that has no stored record yet. Must pass
    /// [`ModuleFactory::validate_settings`]. Default: an empty object.
    fn default_settings(&self) -> Value {
        Value::Object(serde_json::Map::new())
    }

    /// The module's own check of a settings value, run after the common checks
    /// (object root, size, depth, no secret-like keys). The message is shown
    /// to the user and must not contain the value. Default: a module without
    /// settings accepts only an empty object.
    fn validate_settings(&self, settings: &Value) -> Result<(), ModuleFailure> {
        match settings {
            Value::Object(map) if map.is_empty() => Ok(()),
            _ => Err(ModuleFailure::new("у модуля нет настроек")),
        }
    }
}

/// Result of a health check. Informational: it is shown to the user and never
/// starts, stops or restarts anything.
// Module-facing API (ADR-014). No module of a normal build exists yet, and
// the demo modules (#12, feature `demo-modules`) report only `Healthy`.
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Health {
    Healthy,
    Degraded(String),
    Unhealthy(String),
}

/// Error reported by a module from a lifecycle call or a background task.
///
/// The message is shown to the user, so it must not contain secrets, paths or
/// internal details; a module logs those itself.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("{message}")]
pub struct ModuleFailure {
    message: String,
}

impl ModuleFailure {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

/// Stable module identifier in the form `domain.name`, e.g. `ai.lmstudio`.
///
/// Allowed: lowercase latin letters, digits and `_`, in at least two non-empty
/// segments separated by `.`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ModuleId(String);

impl ModuleId {
    pub fn new(id: impl Into<String>) -> Result<Self, ModuleError> {
        let id = id.into();
        if is_valid_id(&id) {
            Ok(Self(id))
        } else {
            Err(ModuleError::InvalidId { id })
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

fn is_valid_id(id: &str) -> bool {
    let mut segments = 0;
    for segment in id.split('.') {
        let valid_chars = segment
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_');
        if segment.is_empty() || !valid_chars {
            return false;
        }
        segments += 1;
    }
    segments >= 2
}

impl fmt::Display for ModuleId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Static description of a module (compile-time module, see ADR-011).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleDescriptor {
    pub id: ModuleId,
    pub display_name: String,
    pub description: String,
    pub version: String,
    /// Required (hard) dependencies.
    pub dependencies: Vec<ModuleId>,
    pub default_enabled: bool,
}

/// Lifecycle phase in which a module failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailurePhase {
    Initialize,
    Start,
    Run,
    Stop,
    HealthCheck,
    /// The settings of an enabled module are invalid; no instance was created
    /// (#10).
    Settings,
}

/// Why a module that the user left enabled cannot run.
// The variant names are part of the agreed contract (#10, ADR-015): every
// reason is about a dependency.
#[allow(clippy::enum_variant_names)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockReason {
    /// A dependency failed while running (set by the lifecycle manager, #9).
    DependencyFailed,
    /// A dependency is disabled (possible when configuration is loaded, #10).
    DependencyDisabled,
    /// The configuration of a dependency could not be read, so the user's
    /// decision about it is unknown (#10).
    DependencyConfigUnavailable,
}

/// Actual state of a module (what is happening now).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModuleState {
    Stopped,
    Initializing,
    Starting,
    Running,
    Stopping,
    Failed {
        phase: FailurePhase,
        /// User-safe message. Panic payloads and paths go to the log only (#9).
        error: String,
        /// Unix time in milliseconds.
        at: u64,
    },
    Blocked {
        /// Root-cause module (start of the chain), not the nearest neighbour.
        by: ModuleId,
        reason: BlockReason,
    },
    /// The configuration record of the module could not be read at startup:
    /// the user's decision is unknown, so the module does not start and
    /// accepts no action. In MM-5A it stays so for the rest of the run; the
    /// next run reads the record again (#10, ADR-015).
    ConfigUnavailable {
        /// User-safe message. Store details go to the log only.
        error: String,
        /// Unix time in milliseconds.
        at: u64,
    },
}

impl ModuleState {
    /// The module has (or is getting/losing) a running instance.
    pub fn is_active(&self) -> bool {
        matches!(
            self,
            Self::Initializing | Self::Starting | Self::Running | Self::Stopping
        )
    }
}

/// Module status: the user's decision (`enabled`) is kept apart from the
/// actual state (`state`). A failed module stays enabled; only the user changes
/// `enabled`, and nothing leaves `Failed`/`Blocked` without a confirmed `recover`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleStatus {
    /// The user's decision. `None` — unknown: the configuration is not loaded
    /// yet (only before the manager is ready) or its record could not be read
    /// (`ConfigUnavailable`).
    pub enabled: Option<bool>,
    pub state: ModuleState,
}

impl Default for ModuleStatus {
    /// Disabled and stopped: the status of a module unknown to a snapshot.
    fn default() -> Self {
        Self {
            enabled: Some(false),
            state: ModuleState::Stopped,
        }
    }
}

impl ModuleStatus {
    /// Status consistency invariant (decision 23 in #13, extended in #10):
    /// - a module that is initializing, starting, running, stopping or blocked
    ///   is enabled (`Some(true)`);
    /// - `ConfigUnavailable` goes with an unknown decision (`None`);
    /// - an unknown decision goes only with `ConfigUnavailable`, or with
    ///   `Stopped` before the configuration is loaded (the manager checks that
    ///   this happens only before it is ready).
    pub fn is_consistent(&self) -> bool {
        match (&self.state, self.enabled) {
            (ModuleState::ConfigUnavailable { .. }, enabled) => enabled.is_none(),
            (ModuleState::Stopped, None) => true,
            (_, None) => false,
            (state, Some(enabled)) => {
                enabled || !(state.is_active() || matches!(state, ModuleState::Blocked { .. }))
            }
        }
    }
}

/// Errors of the module contract, graph and change plans.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ModuleError {
    #[error(
        "invalid module id `{id}`: expected `domain.name` with lowercase latin letters, digits and `_`"
    )]
    InvalidId { id: String },

    #[error("duplicate module id `{0}`")]
    DuplicateId(ModuleId),

    #[error("module `{module}` depends on unknown module `{dependency}`")]
    MissingDependency {
        module: ModuleId,
        dependency: ModuleId,
    },

    #[error("module `{0}` depends on itself")]
    SelfDependency(ModuleId),

    #[error("dependency cycle: {}", display_path(.path))]
    DependencyCycle { path: Vec<ModuleId> },

    #[error("unknown module `{0}`")]
    UnknownModule(ModuleId),

    #[error("cannot {action} module `{module}` in its current status")]
    ActionNotApplicable {
        module: ModuleId,
        action: ChangeAction,
    },

    /// The settings were refused by the common checks or by the module's own
    /// validator (#10). `reason` names the problem, never the value.
    #[error("invalid settings of module `{module}`: {reason}")]
    InvalidSettings { module: ModuleId, reason: String },
}

fn display_path(path: &[ModuleId]) -> String {
    path.iter()
        .map(ModuleId::as_str)
        .collect::<Vec<_>>()
        .join(" -> ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn module_id_accepts_domain_name_format() {
        for id in ["ai.lmstudio", "voice.stt", "demo.base_2", "a.b.c", "x1.y2"] {
            assert_eq!(ModuleId::new(id).expect(id).as_str(), id);
        }
    }

    #[test]
    fn module_id_rejects_invalid_format() {
        for id in [
            "", "ai", ".ai", "ai.", "ai..stt", "AI.stt", "ai.Stt", "ai-x.stt", "ai.stt ", "ай.stt",
        ] {
            assert_eq!(
                ModuleId::new(id),
                Err(ModuleError::InvalidId { id: id.to_owned() }),
                "{id:?} must be rejected"
            );
        }
    }

    #[test]
    fn default_status_is_disabled_and_stopped() {
        let status = ModuleStatus::default();
        assert_eq!(status.enabled, Some(false));
        assert_eq!(status.state, ModuleState::Stopped);
    }

    #[test]
    fn active_states() {
        for state in [
            ModuleState::Initializing,
            ModuleState::Starting,
            ModuleState::Running,
            ModuleState::Stopping,
        ] {
            assert!(state.is_active(), "{state:?}");
        }
        assert!(!ModuleState::Stopped.is_active());
        let blocked = ModuleState::Blocked {
            by: ModuleId::new("t.a").expect("valid"),
            reason: BlockReason::DependencyDisabled,
        };
        assert!(!blocked.is_active());
        let failed = ModuleState::Failed {
            phase: FailurePhase::Run,
            error: "x".into(),
            at: 0,
        };
        assert!(!failed.is_active());
    }

    #[test]
    fn cycle_error_lists_path() {
        let id = |s: &str| ModuleId::new(s).expect("valid");
        let err = ModuleError::DependencyCycle {
            path: vec![id("a.x"), id("b.y"), id("a.x")],
        };
        assert_eq!(err.to_string(), "dependency cycle: a.x -> b.y -> a.x");
    }

    #[test]
    fn status_consistency_invariant() {
        let status = |enabled, state: &ModuleState| ModuleStatus {
            enabled,
            state: state.clone(),
        };
        let blocked = |reason| ModuleState::Blocked {
            by: ModuleId::new("t.a").expect("valid"),
            reason,
        };
        let failed = ModuleState::Failed {
            phase: FailurePhase::Settings,
            error: "x".into(),
            at: 0,
        };
        let unavailable = ModuleState::ConfigUnavailable {
            error: "x".into(),
            at: 0,
        };
        // Active and blocked: only enabled.
        for state in [
            ModuleState::Initializing,
            ModuleState::Starting,
            ModuleState::Running,
            ModuleState::Stopping,
            blocked(BlockReason::DependencyFailed),
            blocked(BlockReason::DependencyDisabled),
            blocked(BlockReason::DependencyConfigUnavailable),
        ] {
            assert!(status(Some(true), &state).is_consistent(), "{state:?}");
            assert!(!status(Some(false), &state).is_consistent(), "{state:?}");
            assert!(!status(None, &state).is_consistent(), "{state:?}");
        }
        // Stopped and failed: enabled or disabled.
        for state in [ModuleState::Stopped, failed.clone()] {
            assert!(status(Some(true), &state).is_consistent());
            assert!(status(Some(false), &state).is_consistent());
        }
        // Unknown decision: with `ConfigUnavailable`, or `Stopped` before
        // loading; never with anything else.
        assert!(status(None, &unavailable).is_consistent());
        assert!(status(None, &ModuleState::Stopped).is_consistent());
        assert!(!status(None, &failed).is_consistent());
        // `ConfigUnavailable` only with an unknown decision.
        assert!(!status(Some(true), &unavailable).is_consistent());
        assert!(!status(Some(false), &unavailable).is_consistent());
        assert!(!unavailable.is_active());
    }

    #[test]
    fn default_settings_methods_accept_only_an_empty_object() {
        struct Plain(ModuleDescriptor);
        impl ModuleFactory for Plain {
            fn descriptor(&self) -> &ModuleDescriptor {
                &self.0
            }
            fn create(&self) -> Box<dyn Module> {
                unreachable!("not created in this test")
            }
        }
        let factory = Plain(ModuleDescriptor {
            id: ModuleId::new("t.a").expect("valid"),
            display_name: "a".into(),
            description: String::new(),
            version: "0".into(),
            dependencies: Vec::new(),
            default_enabled: false,
        });
        let defaults = factory.default_settings();
        assert_eq!(defaults, serde_json::json!({}));
        assert_eq!(factory.validate_settings(&defaults), Ok(()));
        assert!(
            factory
                .validate_settings(&serde_json::json!({"x": 1}))
                .is_err()
        );
        assert!(factory.validate_settings(&serde_json::json!([])).is_err());
    }

    #[test]
    fn module_failure_message_is_shown_as_is() {
        let failure = ModuleFailure::new("no microphone");
        assert_eq!(failure.message(), "no microphone");
        assert_eq!(failure.to_string(), "no microphone");
    }
}
