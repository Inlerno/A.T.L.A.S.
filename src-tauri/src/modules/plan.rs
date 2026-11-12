//! Two-phase change plans (issue #7, ADR-012).
//!
//! A plan is computed from the dependency graph and a snapshot of statuses
//! without side effects. The user sees the affected modules and confirms; the
//! lifecycle manager (#9) recomputes the plan and applies it only if
//! [`verify`] approves the confirmation. Nothing here restarts anything on its
//! own: leaving `Failed`/`Blocked` always requires a confirmed `recover`.
//!
//! An unknown user decision (`enabled: None`, #10) is never guessed: no action
//! applies to such a module, it blocks `enable` and `recover` of the modules
//! that depend on it, and the `disable` and `restart` cascades leave it out.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use super::contract::{ModuleError, ModuleId, ModuleState, ModuleStatus};
use super::graph::ModuleGraph;

/// Snapshot of module statuses. Modules missing from the map are treated as
/// disabled and stopped ([`ModuleStatus::default`]).
pub type StatusMap = BTreeMap<ModuleId, ModuleStatus>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeAction {
    Enable,
    Disable,
    Recover,
    Restart,
}

impl fmt::Display for ChangeAction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Enable => "enable",
            Self::Disable => "disable",
            Self::Recover => "recover",
            Self::Restart => "restart",
        })
    }
}

/// One ordered step of a plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanStep {
    /// Set `enabled = true` (user decision).
    Enable(ModuleId),
    /// Create a new instance and start it (initialize + start in #9).
    Start(ModuleId),
    /// Stop the running instance and release it.
    Stop(ModuleId),
    /// Set `enabled = false` (user decision); the module ends up `Stopped`.
    Disable(ModuleId),
}

/// A module that prevents the plan from being applied, with its current state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Blocker {
    pub id: ModuleId,
    pub state: ModuleState,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangePlan {
    pub action: ChangeAction,
    pub target: ModuleId,
    /// Modules affected besides the target, sorted. This is the list the user
    /// confirms. Empty when the plan is not applicable.
    pub affected: Vec<ModuleId>,
    /// Ordered steps. Empty when the plan is not applicable.
    pub steps: Vec<PlanStep>,
    /// `recover`: always. Other actions: when `affected` is not empty.
    pub requires_confirmation: bool,
    /// Non-empty when the plan cannot be applied (e.g. a dependency is
    /// `Failed`/`Blocked` and must be recovered first).
    pub blockers: Vec<Blocker>,
}

impl ChangePlan {
    pub fn is_applicable(&self) -> bool {
        self.blockers.is_empty()
    }
}

/// The user's confirmation sent with `apply`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Confirmation {
    pub confirmed: bool,
    /// The `affected` list the user saw and confirmed.
    pub acknowledged: Vec<ModuleId>,
}

/// Result of checking a confirmation against the freshly computed plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verification {
    Approved,
    /// The plan has blockers; nothing may be applied.
    NotApplicable(ChangePlan),
    /// The plan requires confirmation and none was given.
    ConfirmationRequired(ChangePlan),
    /// The confirmed list differs from the current `affected`: show the new plan.
    ImpactChanged(ChangePlan),
}

/// Computes the plan for `action` on `target`.
///
/// Returns an error for an unknown module or when the action does not apply to
/// the target's current status (e.g. `enable` on an enabled module).
pub fn plan(
    graph: &ModuleGraph,
    statuses: &StatusMap,
    action: ChangeAction,
    target: &ModuleId,
) -> Result<ChangePlan, ModuleError> {
    if !graph.contains(target) {
        return Err(ModuleError::UnknownModule(target.clone()));
    }
    let status = status_of(statuses, target);
    let applicable = match (action, status.enabled) {
        // The user's decision is unknown: nothing applies (#10).
        (_, None) => false,
        (ChangeAction::Enable, Some(enabled)) => !enabled,
        (ChangeAction::Disable, Some(enabled)) => enabled,
        (ChangeAction::Recover, Some(_)) => matches!(
            status.state,
            ModuleState::Failed { .. } | ModuleState::Blocked { .. }
        ),
        (ChangeAction::Restart, Some(enabled)) => enabled && status.state == ModuleState::Running,
    };
    if !applicable {
        return Err(ModuleError::ActionNotApplicable {
            module: target.clone(),
            action,
        });
    }

    let mut builder = Builder::default();
    match action {
        ChangeAction::Enable => {
            builder.bring_up_dependencies(graph, statuses, target);
            builder.step(PlanStep::Enable(target.clone()));
            builder.step(PlanStep::Start(target.clone()));
        }
        ChangeAction::Disable => {
            let dependents: BTreeSet<ModuleId> = graph
                .transitive_dependents(target)
                .into_iter()
                .filter(|m| status_of(statuses, m).enabled == Some(true))
                .collect();
            let mut order_set = dependents.clone();
            order_set.insert(target.clone());
            for module in graph.in_shutdown_order(&order_set) {
                if status_of(statuses, &module).state.is_active() {
                    builder.step(PlanStep::Stop(module.clone()));
                }
                builder.step(PlanStep::Disable(module));
            }
            builder.affected = dependents;
        }
        ChangeAction::Recover => {
            builder.bring_up_dependencies(graph, statuses, target);
            if status.enabled == Some(false) {
                builder.step(PlanStep::Enable(target.clone()));
            }
            builder.step(PlanStep::Start(target.clone()));
            builder.restored.insert(target.clone());
            builder.ready.insert(target.clone());
            builder.restart_blocked_dependents(graph, statuses, target);
        }
        ChangeAction::Restart => {
            let dependents: BTreeSet<ModuleId> = graph
                .transitive_dependents(target)
                .into_iter()
                .filter(|m| {
                    let s = status_of(statuses, m);
                    s.enabled == Some(true) && s.state == ModuleState::Running
                })
                .collect();
            let mut order_set = dependents.clone();
            order_set.insert(target.clone());
            for module in graph.in_shutdown_order(&order_set) {
                builder.step(PlanStep::Stop(module));
            }
            for module in graph.in_start_order(&order_set) {
                builder.step(PlanStep::Start(module));
            }
            builder.affected = dependents;
        }
    }
    Ok(builder.finish(action, target.clone()))
}

/// Checks the user's confirmation against `current`, the plan recomputed at
/// apply time.
pub fn verify(current: ChangePlan, confirmation: &Confirmation) -> Verification {
    if !current.is_applicable() {
        return Verification::NotApplicable(current);
    }
    if current.requires_confirmation && !confirmation.confirmed {
        return Verification::ConfirmationRequired(current);
    }
    let acknowledged: BTreeSet<&ModuleId> = confirmation.acknowledged.iter().collect();
    let affected: BTreeSet<&ModuleId> = current.affected.iter().collect();
    if acknowledged != affected {
        return Verification::ImpactChanged(current);
    }
    Verification::Approved
}

/// Statuses expected after all steps of `plan` succeed.
///
/// Used to show the outcome before confirmation and in tests. A step that fails
/// at runtime yields `Failed`/`Blocked` instead (#9).
pub fn expected_statuses(plan: &ChangePlan, statuses: &StatusMap) -> StatusMap {
    let mut result = statuses.clone();
    for step in &plan.steps {
        match step {
            PlanStep::Enable(id) => result.entry(id.clone()).or_default().enabled = Some(true),
            PlanStep::Disable(id) => {
                let status = result.entry(id.clone()).or_default();
                status.enabled = Some(false);
                status.state = ModuleState::Stopped;
            }
            PlanStep::Start(id) => {
                result.entry(id.clone()).or_default().state = ModuleState::Running
            }
            PlanStep::Stop(id) => {
                result.entry(id.clone()).or_default().state = ModuleState::Stopped
            }
        }
    }
    result
}

fn status_of(statuses: &StatusMap, id: &ModuleId) -> ModuleStatus {
    statuses.get(id).cloned().unwrap_or_default()
}

#[derive(Default)]
struct Builder {
    steps: Vec<PlanStep>,
    affected: BTreeSet<ModuleId>,
    blockers: Vec<Blocker>,
    /// Modules that will be running when the plan completes.
    ready: BTreeSet<ModuleId>,
    /// Modules this plan brings back (newly enabled or started).
    restored: BTreeSet<ModuleId>,
}

impl Builder {
    fn step(&mut self, step: PlanStep) {
        self.steps.push(step);
    }

    /// Ensures every transitive dependency of `target` will be running:
    /// disabled ones are enabled and started (affected), enabled-but-stopped
    /// ones are started (affected), running ones are kept, and anything else
    /// (`Failed`, `Blocked`, `ConfigUnavailable`, in transition) blocks the
    /// plan.
    fn bring_up_dependencies(
        &mut self,
        graph: &ModuleGraph,
        statuses: &StatusMap,
        target: &ModuleId,
    ) {
        let dependencies = graph.transitive_dependencies(target);
        for module in graph.in_start_order(&dependencies) {
            let status = status_of(statuses, &module);
            if status.enabled == Some(false) {
                self.step(PlanStep::Enable(module.clone()));
                self.step(PlanStep::Start(module.clone()));
                self.affected.insert(module.clone());
                self.restored.insert(module.clone());
                self.ready.insert(module);
            } else if status.enabled.is_none() {
                // Unknown decision: never enabled or started implicitly.
                self.blockers.push(Blocker {
                    id: module,
                    state: status.state,
                });
            } else {
                match status.state {
                    ModuleState::Running => {
                        self.ready.insert(module);
                    }
                    ModuleState::Stopped => {
                        self.step(PlanStep::Start(module.clone()));
                        self.affected.insert(module.clone());
                        self.restored.insert(module.clone());
                        self.ready.insert(module);
                    }
                    state => self.blockers.push(Blocker { id: module, state }),
                }
            }
        }
    }

    /// Starts enabled transitive dependents of `target` that are `Blocked` by a
    /// module this plan restores and whose dependencies will all be running.
    fn restart_blocked_dependents(
        &mut self,
        graph: &ModuleGraph,
        statuses: &StatusMap,
        target: &ModuleId,
    ) {
        let dependents = graph.transitive_dependents(target);
        for module in graph.in_start_order(&dependents) {
            let status = status_of(statuses, &module);
            let blocked_by_restored = matches!(
                &status.state,
                ModuleState::Blocked { by, .. } if self.restored.contains(by)
            );
            if status.enabled != Some(true) || !blocked_by_restored {
                continue;
            }
            let dependencies_ready = graph.dependencies(&module).all(|dependency| {
                self.ready.contains(dependency) || {
                    let s = status_of(statuses, dependency);
                    s.enabled == Some(true) && s.state == ModuleState::Running
                }
            });
            if dependencies_ready {
                self.step(PlanStep::Start(module.clone()));
                self.affected.insert(module.clone());
                self.ready.insert(module);
            }
        }
    }

    fn finish(mut self, action: ChangeAction, target: ModuleId) -> ChangePlan {
        if !self.blockers.is_empty() {
            self.steps.clear();
            self.affected.clear();
        }
        let affected: Vec<ModuleId> = self.affected.into_iter().collect();
        let requires_confirmation = action == ChangeAction::Recover || !affected.is_empty();
        ChangePlan {
            action,
            target,
            affected,
            steps: self.steps,
            requires_confirmation,
            blockers: self.blockers,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modules::contract::{BlockReason, FailurePhase, ModuleDescriptor};

    fn id(s: &str) -> ModuleId {
        ModuleId::new(s).expect("valid id")
    }

    fn ids(list: &[&str]) -> Vec<ModuleId> {
        list.iter().map(|s| id(s)).collect()
    }

    fn module(name: &str, deps: &[&str]) -> ModuleDescriptor {
        ModuleDescriptor {
            id: id(name),
            display_name: name.to_owned(),
            description: String::new(),
            version: "0.1.0".to_owned(),
            dependencies: deps.iter().map(|d| id(d)).collect(),
            default_enabled: false,
        }
    }

    /// Chain A <- B <- C (C depends on B, B depends on A).
    fn chain() -> ModuleGraph {
        ModuleGraph::build(vec![
            module("t.a", &[]),
            module("t.b", &["t.a"]),
            module("t.c", &["t.b"]),
        ])
        .expect("valid graph")
    }

    fn running() -> ModuleStatus {
        ModuleStatus {
            enabled: Some(true),
            state: ModuleState::Running,
        }
    }

    fn failed() -> ModuleStatus {
        ModuleStatus {
            enabled: Some(true),
            state: ModuleState::Failed {
                phase: FailurePhase::Run,
                error: "boom".into(),
                at: 1,
            },
        }
    }

    fn blocked(by: &str, reason: BlockReason) -> ModuleStatus {
        ModuleStatus {
            enabled: Some(true),
            state: ModuleState::Blocked { by: id(by), reason },
        }
    }

    fn statuses(entries: &[(&str, ModuleStatus)]) -> StatusMap {
        entries.iter().map(|(m, s)| (id(m), s.clone())).collect()
    }

    fn starts(plan: &ChangePlan) -> Vec<ModuleId> {
        plan.steps
            .iter()
            .filter_map(|s| match s {
                PlanStep::Start(m) => Some(m.clone()),
                _ => None,
            })
            .collect()
    }

    fn stops(plan: &ChangePlan) -> Vec<ModuleId> {
        plan.steps
            .iter()
            .filter_map(|s| match s {
                PlanStep::Stop(m) => Some(m.clone()),
                _ => None,
            })
            .collect()
    }

    // ---- enable ----

    #[test]
    fn enable_brings_up_disabled_dependencies_in_order() {
        let graph = chain();
        let plan = plan(&graph, &StatusMap::new(), ChangeAction::Enable, &id("t.c")).unwrap();
        assert!(plan.is_applicable());
        assert_eq!(plan.affected, ids(&["t.a", "t.b"]));
        assert!(plan.requires_confirmation);
        assert_eq!(
            plan.steps,
            vec![
                PlanStep::Enable(id("t.a")),
                PlanStep::Start(id("t.a")),
                PlanStep::Enable(id("t.b")),
                PlanStep::Start(id("t.b")),
                PlanStep::Enable(id("t.c")),
                PlanStep::Start(id("t.c")),
            ]
        );
    }

    #[test]
    fn enable_with_running_dependencies_needs_no_confirmation() {
        let graph = chain();
        let s = statuses(&[("t.a", running()), ("t.b", running())]);
        let plan = plan(&graph, &s, ChangeAction::Enable, &id("t.c")).unwrap();
        assert!(plan.affected.is_empty());
        assert!(!plan.requires_confirmation);
        assert_eq!(starts(&plan), ids(&["t.c"]));
    }

    #[test]
    fn enable_is_blocked_by_failed_dependency() {
        let graph = chain();
        let s = statuses(&[("t.a", failed())]);
        let plan = plan(&graph, &s, ChangeAction::Enable, &id("t.c")).unwrap();
        assert!(!plan.is_applicable());
        assert_eq!(plan.blockers.len(), 1);
        assert_eq!(plan.blockers[0].id, id("t.a"));
        assert!(plan.steps.is_empty());
        assert!(plan.affected.is_empty());
    }

    #[test]
    fn enable_is_blocked_by_blocked_dependency() {
        let graph = chain();
        let s = statuses(&[
            ("t.a", failed()),
            ("t.b", blocked("t.a", BlockReason::DependencyFailed)),
        ]);
        let plan = plan(&graph, &s, ChangeAction::Enable, &id("t.c")).unwrap();
        let blocker_ids: Vec<ModuleId> = plan.blockers.iter().map(|b| b.id.clone()).collect();
        assert_eq!(blocker_ids, ids(&["t.a", "t.b"]));
    }

    // ---- disable ----

    #[test]
    fn disable_stops_dependents_in_reverse_order() {
        let graph = chain();
        let s = statuses(&[("t.a", running()), ("t.b", running()), ("t.c", running())]);
        let plan = plan(&graph, &s, ChangeAction::Disable, &id("t.a")).unwrap();
        assert_eq!(plan.affected, ids(&["t.b", "t.c"]));
        assert!(plan.requires_confirmation);
        assert_eq!(stops(&plan), ids(&["t.c", "t.b", "t.a"]));
        assert_eq!(
            plan.steps,
            vec![
                PlanStep::Stop(id("t.c")),
                PlanStep::Disable(id("t.c")),
                PlanStep::Stop(id("t.b")),
                PlanStep::Disable(id("t.b")),
                PlanStep::Stop(id("t.a")),
                PlanStep::Disable(id("t.a")),
            ]
        );
    }

    #[test]
    fn disable_without_dependents_needs_no_confirmation() {
        let graph = chain();
        let s = statuses(&[("t.a", running()), ("t.b", running()), ("t.c", running())]);
        let plan = plan(&graph, &s, ChangeAction::Disable, &id("t.c")).unwrap();
        assert!(plan.affected.is_empty());
        assert!(!plan.requires_confirmation);
        assert_eq!(
            plan.steps,
            vec![PlanStep::Stop(id("t.c")), PlanStep::Disable(id("t.c"))]
        );
    }

    #[test]
    fn disable_of_failed_module_disables_blocked_dependents_without_stop() {
        let graph = chain();
        let s = statuses(&[
            ("t.a", failed()),
            ("t.b", blocked("t.a", BlockReason::DependencyFailed)),
            ("t.c", blocked("t.a", BlockReason::DependencyFailed)),
        ]);
        let plan = plan(&graph, &s, ChangeAction::Disable, &id("t.a")).unwrap();
        assert_eq!(plan.affected, ids(&["t.b", "t.c"]));
        assert!(stops(&plan).is_empty(), "nothing is running");
        let after = expected_statuses(&plan, &s);
        for m in ["t.a", "t.b", "t.c"] {
            assert_eq!(after[&id(m)], ModuleStatus::default(), "{m}");
        }
    }

    #[test]
    fn user_disable_makes_dependents_disabled_not_blocked() {
        let graph = chain();
        let s = statuses(&[("t.a", running()), ("t.b", running()), ("t.c", running())]);
        let plan = plan(&graph, &s, ChangeAction::Disable, &id("t.a")).unwrap();
        let after = expected_statuses(&plan, &s);
        for m in ["t.a", "t.b", "t.c"] {
            let status = &after[&id(m)];
            assert_eq!(status.enabled, Some(false), "{m} must be disabled");
            assert_eq!(
                status.state,
                ModuleState::Stopped,
                "{m} must be stopped, not blocked"
            );
        }
    }

    // ---- recover ----

    #[test]
    fn recover_restarts_target_then_blocked_dependents() {
        let graph = chain();
        let s = statuses(&[
            ("t.a", failed()),
            ("t.b", blocked("t.a", BlockReason::DependencyFailed)),
            ("t.c", blocked("t.a", BlockReason::DependencyFailed)),
        ]);
        let plan = plan(&graph, &s, ChangeAction::Recover, &id("t.a")).unwrap();
        assert!(plan.is_applicable());
        assert!(plan.requires_confirmation);
        assert_eq!(plan.affected, ids(&["t.b", "t.c"]));
        assert_eq!(
            plan.steps,
            vec![
                PlanStep::Start(id("t.a")),
                PlanStep::Start(id("t.b")),
                PlanStep::Start(id("t.c")),
            ]
        );
        let after = expected_statuses(&plan, &s);
        for m in ["t.a", "t.b", "t.c"] {
            assert_eq!(after[&id(m)], running(), "{m}");
        }
    }

    #[test]
    fn recover_of_single_module_always_requires_confirmation() {
        let graph = ModuleGraph::build(vec![module("t.solo", &[])]).unwrap();
        let s = statuses(&[("t.solo", failed())]);
        let plan = plan(&graph, &s, ChangeAction::Recover, &id("t.solo")).unwrap();
        assert!(plan.affected.is_empty());
        assert!(plan.requires_confirmation);
        assert_eq!(plan.steps, vec![PlanStep::Start(id("t.solo"))]);
    }

    #[test]
    fn recover_applies_to_failure_in_every_phase() {
        let graph = ModuleGraph::build(vec![module("t.solo", &[])]).unwrap();
        for phase in [
            FailurePhase::Initialize,
            FailurePhase::Start,
            FailurePhase::Run,
            FailurePhase::Stop,
            FailurePhase::HealthCheck,
        ] {
            let s = statuses(&[(
                "t.solo",
                ModuleStatus {
                    enabled: Some(true),
                    state: ModuleState::Failed {
                        phase,
                        error: "x".into(),
                        at: 0,
                    },
                },
            )]);
            let plan = plan(&graph, &s, ChangeAction::Recover, &id("t.solo")).unwrap();
            assert_eq!(plan.steps, vec![PlanStep::Start(id("t.solo"))], "{phase:?}");
        }
    }

    #[test]
    fn recover_of_disabled_module_that_failed_to_stop_enables_it() {
        // `disable` whose stop failed: enabled = false, Failed { phase: Stop }.
        let graph = ModuleGraph::build(vec![module("t.solo", &[])]).unwrap();
        let s = statuses(&[(
            "t.solo",
            ModuleStatus {
                enabled: Some(false),
                state: ModuleState::Failed {
                    phase: FailurePhase::Stop,
                    error: "stop timed out".into(),
                    at: 0,
                },
            },
        )]);
        let plan = plan(&graph, &s, ChangeAction::Recover, &id("t.solo")).unwrap();
        assert_eq!(
            plan.steps,
            vec![
                PlanStep::Enable(id("t.solo")),
                PlanStep::Start(id("t.solo"))
            ]
        );
    }

    #[test]
    fn dependency_in_transition_blocks_the_plan() {
        let graph = chain();
        for state in [
            ModuleState::Initializing,
            ModuleState::Starting,
            ModuleState::Stopping,
        ] {
            let s = statuses(&[(
                "t.a",
                ModuleStatus {
                    enabled: Some(true),
                    state: state.clone(),
                },
            )]);
            let plan = plan(&graph, &s, ChangeAction::Enable, &id("t.b")).unwrap();
            assert!(!plan.is_applicable(), "{state:?}");
            assert_eq!(
                plan.blockers,
                vec![Blocker {
                    id: id("t.a"),
                    state
                }]
            );
        }
    }

    #[test]
    fn recover_of_module_blocked_by_disabled_dependency_enables_it() {
        let graph = chain();
        // t.b is enabled but blocked because t.a is disabled (e.g. loaded config, #10);
        // t.c is blocked by the same root cause.
        let s = statuses(&[
            ("t.b", blocked("t.a", BlockReason::DependencyDisabled)),
            ("t.c", blocked("t.a", BlockReason::DependencyDisabled)),
        ]);
        let plan = plan(&graph, &s, ChangeAction::Recover, &id("t.b")).unwrap();
        assert!(plan.is_applicable());
        assert_eq!(
            plan.steps,
            vec![
                PlanStep::Enable(id("t.a")),
                PlanStep::Start(id("t.a")),
                PlanStep::Start(id("t.b")),
                PlanStep::Start(id("t.c")),
            ]
        );
        assert_eq!(plan.affected, ids(&["t.a", "t.c"]));
    }

    #[test]
    fn recover_is_blocked_while_dependency_failed() {
        let graph = chain();
        let s = statuses(&[
            ("t.a", failed()),
            ("t.b", blocked("t.a", BlockReason::DependencyFailed)),
        ]);
        let plan = plan(&graph, &s, ChangeAction::Recover, &id("t.b")).unwrap();
        assert!(!plan.is_applicable());
        assert_eq!(plan.blockers[0].id, id("t.a"));
        assert!(plan.steps.is_empty());
    }

    #[test]
    fn recover_does_not_start_dependents_with_other_unresolved_dependencies() {
        // t.d depends on t.a and t.x; t.x has failed independently.
        let graph = ModuleGraph::build(vec![
            module("t.a", &[]),
            module("t.x", &[]),
            module("t.d", &["t.a", "t.x"]),
        ])
        .unwrap();
        let s = statuses(&[
            ("t.a", failed()),
            ("t.x", failed()),
            ("t.d", blocked("t.a", BlockReason::DependencyFailed)),
        ]);
        let plan = plan(&graph, &s, ChangeAction::Recover, &id("t.a")).unwrap();
        assert_eq!(plan.steps, vec![PlanStep::Start(id("t.a"))]);
        assert!(plan.affected.is_empty());
    }

    // ---- restart ----

    #[test]
    fn restart_stops_dependents_in_reverse_and_starts_in_order() {
        let graph = chain();
        let s = statuses(&[("t.a", running()), ("t.b", running()), ("t.c", running())]);
        let plan = plan(&graph, &s, ChangeAction::Restart, &id("t.a")).unwrap();
        assert_eq!(plan.affected, ids(&["t.b", "t.c"]));
        assert!(plan.requires_confirmation);
        assert_eq!(stops(&plan), ids(&["t.c", "t.b", "t.a"]));
        assert_eq!(starts(&plan), ids(&["t.a", "t.b", "t.c"]));
        assert_eq!(expected_statuses(&plan, &s), s, "everything runs again");
    }

    #[test]
    fn restart_of_leaf_needs_no_confirmation() {
        let graph = chain();
        let s = statuses(&[("t.a", running()), ("t.b", running()), ("t.c", running())]);
        let plan = plan(&graph, &s, ChangeAction::Restart, &id("t.c")).unwrap();
        assert!(!plan.requires_confirmation);
        assert_eq!(
            plan.steps,
            vec![PlanStep::Stop(id("t.c")), PlanStep::Start(id("t.c"))]
        );
    }

    // ---- applicability ----

    #[test]
    fn actions_not_applicable_to_current_status_are_errors() {
        let graph = chain();
        let s = statuses(&[("t.a", running()), ("t.b", failed())]);
        let cases = [
            (ChangeAction::Enable, "t.a"),  // already enabled
            (ChangeAction::Disable, "t.c"), // already disabled
            (ChangeAction::Recover, "t.a"), // running, nothing to recover
            (ChangeAction::Restart, "t.b"), // failed, use recover
            (ChangeAction::Restart, "t.c"), // not running
        ];
        for (action, target) in cases {
            assert_eq!(
                plan(&graph, &s, action, &id(target)),
                Err(ModuleError::ActionNotApplicable {
                    module: id(target),
                    action,
                }),
                "{action} {target}"
            );
        }
    }

    #[test]
    fn unknown_target_is_an_error() {
        let graph = chain();
        assert_eq!(
            plan(
                &graph,
                &StatusMap::new(),
                ChangeAction::Enable,
                &id("t.nope")
            ),
            Err(ModuleError::UnknownModule(id("t.nope")))
        );
    }

    // ---- verify ----

    #[test]
    fn verify_requires_confirmation_when_plan_needs_it() {
        let graph = chain();
        let current = plan(&graph, &StatusMap::new(), ChangeAction::Enable, &id("t.c")).unwrap();
        let verdict = verify(current.clone(), &Confirmation::default());
        assert_eq!(verdict, Verification::ConfirmationRequired(current));
    }

    #[test]
    fn verify_rejects_stale_acknowledged_list() {
        let graph = chain();
        // The user confirmed while only t.a was disabled...
        let seen = statuses(&[("t.b", running())]);
        let shown = plan(&graph, &seen, ChangeAction::Enable, &id("t.c")).unwrap();
        assert_eq!(shown.affected, ids(&["t.a"]));
        // ...but by apply time t.b got disabled too.
        let current = plan(&graph, &StatusMap::new(), ChangeAction::Enable, &id("t.c")).unwrap();
        let confirmation = Confirmation {
            confirmed: true,
            acknowledged: shown.affected.clone(),
        };
        assert_eq!(
            verify(current.clone(), &confirmation),
            Verification::ImpactChanged(current)
        );
    }

    #[test]
    fn verify_approves_matching_confirmation() {
        let graph = chain();
        let current = plan(&graph, &StatusMap::new(), ChangeAction::Enable, &id("t.c")).unwrap();
        let confirmation = Confirmation {
            confirmed: true,
            acknowledged: vec![id("t.b"), id("t.a")], // order does not matter
        };
        assert_eq!(verify(current, &confirmation), Verification::Approved);
    }

    #[test]
    fn verify_recover_without_affected_still_needs_confirmation() {
        let graph = ModuleGraph::build(vec![module("t.solo", &[])]).unwrap();
        let s = statuses(&[("t.solo", failed())]);
        let current = plan(&graph, &s, ChangeAction::Recover, &id("t.solo")).unwrap();
        assert_eq!(
            verify(current.clone(), &Confirmation::default()),
            Verification::ConfirmationRequired(current.clone())
        );
        let confirmed = Confirmation {
            confirmed: true,
            acknowledged: vec![],
        };
        assert_eq!(verify(current, &confirmed), Verification::Approved);
    }

    #[test]
    fn verify_reports_not_applicable_plan() {
        let graph = chain();
        let s = statuses(&[("t.a", failed())]);
        let current = plan(&graph, &s, ChangeAction::Enable, &id("t.c")).unwrap();
        let confirmation = Confirmation {
            confirmed: true,
            acknowledged: vec![],
        };
        assert_eq!(
            verify(current.clone(), &confirmation),
            Verification::NotApplicable(current)
        );
    }

    #[test]
    fn verify_without_confirmation_need_and_stale_list_reports_impact_change() {
        let graph = chain();
        let s = statuses(&[("t.a", running()), ("t.b", running())]);
        let current = plan(&graph, &s, ChangeAction::Enable, &id("t.c")).unwrap();
        assert!(!current.requires_confirmation);
        let stale = Confirmation {
            confirmed: false,
            acknowledged: vec![id("t.a")],
        };
        assert_eq!(
            verify(current.clone(), &stale),
            Verification::ImpactChanged(current.clone())
        );
        assert_eq!(
            verify(current, &Confirmation::default()),
            Verification::Approved
        );
    }

    // ---- state transitions (expected outcome of plans) ----

    #[test]
    fn expected_statuses_of_enable_and_recover() {
        let graph = chain();
        let plan_enable =
            plan(&graph, &StatusMap::new(), ChangeAction::Enable, &id("t.c")).unwrap();
        let after = expected_statuses(&plan_enable, &StatusMap::new());
        for m in ["t.a", "t.b", "t.c"] {
            assert_eq!(after[&id(m)], running(), "{m}");
        }

        let s = statuses(&[("t.a", failed())]);
        let plan_recover = plan(&graph, &s, ChangeAction::Recover, &id("t.a")).unwrap();
        assert_eq!(expected_statuses(&plan_recover, &s)[&id("t.a")], running());
    }

    #[test]
    fn planning_has_no_side_effects() {
        let graph = chain();
        let s = statuses(&[("t.a", running()), ("t.b", running()), ("t.c", running())]);
        let before = s.clone();
        for action in [ChangeAction::Disable, ChangeAction::Restart] {
            let _ = plan(&graph, &s, action, &id("t.a")).unwrap();
        }
        assert_eq!(s, before);
    }

    // ---- unknown user decision (#10) ----

    fn unavailable() -> ModuleStatus {
        ModuleStatus {
            enabled: None,
            state: ModuleState::ConfigUnavailable {
                error: "x".into(),
                at: 0,
            },
        }
    }

    #[test]
    fn a_module_with_unavailable_config_accepts_no_action() {
        let graph = chain();
        let s = statuses(&[("t.b", unavailable())]);
        for action in [
            ChangeAction::Enable,
            ChangeAction::Disable,
            ChangeAction::Recover,
            ChangeAction::Restart,
        ] {
            assert_eq!(
                plan(&graph, &s, action, &id("t.b")),
                Err(ModuleError::ActionNotApplicable {
                    module: id("t.b"),
                    action
                }),
                "{action}"
            );
        }
        // Not loaded yet (before the manager is ready): the same.
        let not_loaded = statuses(&[(
            "t.b",
            ModuleStatus {
                enabled: None,
                state: ModuleState::Stopped,
            },
        )]);
        assert!(plan(&graph, &not_loaded, ChangeAction::Enable, &id("t.b")).is_err());
    }

    #[test]
    fn an_unavailable_dependency_blocks_enable_and_recover() {
        let graph = chain();
        let s = statuses(&[("t.a", unavailable())]);
        let enable = plan(&graph, &s, ChangeAction::Enable, &id("t.c")).unwrap();
        assert!(!enable.is_applicable());
        assert_eq!(
            enable.blockers,
            vec![Blocker {
                id: id("t.a"),
                state: unavailable().state
            }]
        );
        assert!(enable.steps.is_empty() && enable.affected.is_empty());

        let s = statuses(&[
            ("t.a", unavailable()),
            (
                "t.b",
                blocked("t.a", BlockReason::DependencyConfigUnavailable),
            ),
        ]);
        let recover = plan(&graph, &s, ChangeAction::Recover, &id("t.b")).unwrap();
        assert!(!recover.is_applicable());
        assert_eq!(recover.blockers[0].id, id("t.a"));
    }

    #[test]
    fn disable_and_restart_skip_modules_with_unavailable_config() {
        let graph = ModuleGraph::build(vec![
            module("t.a", &[]),
            module("t.b", &["t.a"]),
            module("t.c", &["t.a"]),
        ])
        .unwrap();
        let s = statuses(&[
            ("t.a", running()),
            ("t.b", unavailable()),
            ("t.c", running()),
        ]);
        let disable = plan(&graph, &s, ChangeAction::Disable, &id("t.a")).unwrap();
        assert_eq!(disable.affected, ids(&["t.c"]));
        assert!(
            disable.steps.iter().all(|step| !matches!(
                step,
                PlanStep::Stop(m) | PlanStep::Disable(m) if m == &id("t.b")
            )),
            "{:?}",
            disable.steps
        );
        let restart = plan(&graph, &s, ChangeAction::Restart, &id("t.a")).unwrap();
        assert_eq!(restart.affected, ids(&["t.c"]));
        assert!(!starts(&restart).contains(&id("t.b")));
        assert!(!stops(&restart).contains(&id("t.b")));
    }
}
