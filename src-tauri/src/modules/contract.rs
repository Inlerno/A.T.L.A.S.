//! Module contract types (issue #7): identifiers, descriptors, statuses, errors.
//!
//! Pure data: no async and no runtime. The `Module` / `ModuleFactory` traits and
//! `Health` are introduced with the lifecycle manager (#9). Status changes are
//! published through the central typed event bus (#8) using these types.

use std::fmt;

use thiserror::Error;

use super::plan::ChangeAction;

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
}

/// Why a module that the user left enabled cannot run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockReason {
    /// A dependency failed while running (set by the lifecycle manager, #9).
    DependencyFailed,
    /// A dependency is disabled (possible when configuration is loaded, #10).
    DependencyDisabled,
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
    pub enabled: bool,
    pub state: ModuleState,
}

impl Default for ModuleStatus {
    /// Disabled and stopped: the status of a module unknown to a snapshot.
    fn default() -> Self {
        Self {
            enabled: false,
            state: ModuleState::Stopped,
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
        assert!(!status.enabled);
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
}
