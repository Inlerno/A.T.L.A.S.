//! Commands of the module manager (issue #11).
//!
//! Each Tauri command is a one-line wrapper around a function that takes the
//! manager handle, so the same code is tested without a webview. Expected
//! outcomes of a change come back as [`ApplyResultDto`]; [`ErrorDto`] is only
//! for real errors.

use serde_json::Value;
use tauri::State;

use crate::app::AppState;
use crate::dto::ErrorDto;
use crate::dto::modules::{
    ApplyResultDto, ChangeActionDto, ChangePlanDto, HealthDto, ModuleDto, ModuleSettingsDto,
};
use crate::modules::contract::ModuleId;
use crate::modules::manager::{ManagerError, ModuleManagerHandle};
use crate::modules::plan::Confirmation;

type Modules<'a> = State<'a, AppState>;

/// Parses a module id received from the webview.
fn module_id(id: String) -> Result<ModuleId, ErrorDto> {
    ModuleId::new(id).map_err(ErrorDto::from)
}

/// Maps the manager's answer to the IPC result and records the outcome at
/// debug level: the command and the error code, never arguments or values.
fn answer<T, D: From<T>>(
    command: &'static str,
    result: Result<T, ManagerError>,
) -> Result<D, ErrorDto> {
    match result {
        Ok(value) => {
            tracing::debug!(command, "ipc command ok");
            Ok(value.into())
        }
        Err(error) => {
            let error = ErrorDto::from(error);
            tracing::debug!(command, code = error.code, "ipc command failed");
            Err(error)
        }
    }
}

pub(crate) fn list(modules: &ModuleManagerHandle) -> Result<Vec<ModuleDto>, ErrorDto> {
    let views = answer::<_, Vec<_>>("list_modules", modules.modules())?;
    Ok(views.into_iter().map(ModuleDto::from).collect())
}

pub(crate) fn get(modules: &ModuleManagerHandle, id: String) -> Result<ModuleDto, ErrorDto> {
    answer("get_module", modules.module(&module_id(id)?))
}

pub(crate) fn plan(
    modules: &ModuleManagerHandle,
    id: String,
    action: ChangeActionDto,
) -> Result<ChangePlanDto, ErrorDto> {
    answer(
        "plan_module_change",
        modules.plan(action.into(), &module_id(id)?),
    )
}

/// The confirmation is checked by the manager against the plan as it is at
/// that moment — never only in the UI; `recover` always needs `confirmed`.
pub(crate) async fn apply(
    modules: &ModuleManagerHandle,
    id: String,
    action: ChangeActionDto,
    confirmed: bool,
    acknowledged: Vec<String>,
) -> Result<ApplyResultDto, ErrorDto> {
    let acknowledged = acknowledged
        .into_iter()
        .map(module_id)
        .collect::<Result<_, _>>()?;
    let confirmation = Confirmation {
        confirmed,
        acknowledged,
    };
    let outcome = modules.apply(action.into(), module_id(id)?, confirmation);
    answer("apply_module_change", outcome.await)
}

pub(crate) async fn check_health(
    modules: &ModuleManagerHandle,
    id: String,
) -> Result<HealthDto, ErrorDto> {
    answer(
        "check_module_health",
        modules.check_health(module_id(id)?).await,
    )
}

pub(crate) fn get_settings(
    modules: &ModuleManagerHandle,
    id: String,
) -> Result<ModuleSettingsDto, ErrorDto> {
    let id = module_id(id)?;
    let settings = answer::<_, crate::modules::manager::ModuleSettings>(
        "get_module_settings",
        modules.get_settings(&id),
    )?;
    Ok(ModuleSettingsDto::new(&id, settings))
}

pub(crate) async fn update_settings(
    modules: &ModuleManagerHandle,
    id: String,
    settings: Value,
) -> Result<(), ErrorDto> {
    let updated = modules.update_settings(module_id(id)?, settings);
    answer("update_module_settings", updated.await)
}

#[tauri::command]
pub async fn list_modules(state: Modules<'_>) -> Result<Vec<ModuleDto>, ErrorDto> {
    list(state.modules())
}

#[tauri::command]
pub async fn get_module(state: Modules<'_>, id: String) -> Result<ModuleDto, ErrorDto> {
    get(state.modules(), id)
}

#[tauri::command]
pub async fn plan_module_change(
    state: Modules<'_>,
    id: String,
    action: ChangeActionDto,
) -> Result<ChangePlanDto, ErrorDto> {
    plan(state.modules(), id, action)
}

#[tauri::command]
pub async fn apply_module_change(
    state: Modules<'_>,
    id: String,
    action: ChangeActionDto,
    confirmed: bool,
    acknowledged: Vec<String>,
) -> Result<ApplyResultDto, ErrorDto> {
    apply(state.modules(), id, action, confirmed, acknowledged).await
}

#[tauri::command]
pub async fn check_module_health(state: Modules<'_>, id: String) -> Result<HealthDto, ErrorDto> {
    check_health(state.modules(), id).await
}

#[tauri::command]
pub async fn get_module_settings(
    state: Modules<'_>,
    id: String,
) -> Result<ModuleSettingsDto, ErrorDto> {
    get_settings(state.modules(), id)
}

#[tauri::command]
pub async fn update_module_settings(
    state: Modules<'_>,
    id: String,
    settings: Value,
) -> Result<(), ErrorDto> {
    update_settings(state.modules(), id, settings).await
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::dto::code;
    use crate::dto::modules::{BlockReasonDto, BlockerDto, ModuleStateDto, PlanStepDto};
    use crate::modules::contract::Health;
    use crate::modules::plan::ChangeAction;
    use crate::modules::testing::*;

    const ENABLE: ChangeActionDto = ChangeActionDto::Enable;
    const DISABLE: ChangeActionDto = ChangeActionDto::Disable;
    const RECOVER: ChangeActionDto = ChangeActionDto::Recover;

    fn names(ids: &[&str]) -> Vec<String> {
        ids.iter().map(|id| (*id).to_owned()).collect()
    }

    fn error_code<T: std::fmt::Debug>(result: Result<T, ErrorDto>) -> &'static str {
        result.expect_err("an error").code
    }

    #[test]
    fn list_and_get_show_the_snapshot() {
        run(async {
            let h = harness(&[("t.a", &[]), ("t.b", &["t.a"])]).await;
            h.confirm(ChangeAction::Enable, "t.a").await;
            let listed = list(&h.manager).expect("list");
            assert_eq!(listed.len(), 2);
            assert_eq!(listed[0].descriptor.id, "t.a", "in start order");
            assert_eq!(listed[0].enabled, Some(true));
            assert_eq!(listed[0].state, ModuleStateDto::Running);
            assert_eq!(listed[0].health, None, "not checked yet");
            assert_eq!(listed[1].descriptor.dependencies, names(&["t.a"]));
            assert_eq!(listed[1].enabled, Some(false));
            assert_eq!(listed[1].state, ModuleStateDto::Stopped);
            assert_eq!(get(&h.manager, "t.a".into()).expect("get"), listed[0]);
        });
    }

    #[test]
    fn health_and_restart_required_reach_the_module_dto() {
        run(async {
            let h = harness(&[("t.a", &[])]).await;
            h.confirm(ChangeAction::Enable, "t.a").await;
            h.probe("t.a").script(|script| {
                script.health = HealthBehavior::Answer(Health::Degraded("slow".into()));
            });
            let health = check_health(&h.manager, "t.a".into())
                .await
                .expect("health");
            assert_eq!(
                health,
                HealthDto::Degraded {
                    message: "slow".into()
                }
            );
            update_settings(&h.manager, "t.a".into(), json!({"level": 2}))
                .await
                .expect("update");
            let module = get(&h.manager, "t.a".into()).expect("get");
            assert_eq!(module.health, Some(health));
            assert!(module.restart_required);
            assert_eq!(
                module.state,
                ModuleStateDto::Running,
                "not restarted on its own"
            );

            // The instance is gone: so is its health result and the flag.
            h.confirm(ChangeAction::Disable, "t.a").await;
            let module = get(&h.manager, "t.a".into()).expect("get");
            assert_eq!(module.health, None);
            assert!(!module.restart_required);
            assert_eq!(
                error_code(check_health(&h.manager, "t.a".into()).await),
                code::NOT_RUNNING
            );
        });
    }

    /// `recover` without `confirmed`, a stale `acknowledged` list and a plan
    /// with blockers are expected outcomes, never errors (#11).
    #[test]
    fn expected_outcomes_of_apply_are_results_not_errors() {
        run(async {
            let mut h = harness(&[("t.a", &[]), ("t.b", &["t.a"]), ("t.c", &["t.a"])]).await;
            h.confirm(ChangeAction::Enable, "t.b").await;
            assert!(h.probe("t.a").end_background(TaskEnd::Fail));
            settle().await;
            h.events();

            // `recover` always needs `confirmed`, even with nothing affected.
            let plan_a = plan(&h.manager, "t.a".into(), RECOVER).expect("plan");
            assert!(plan_a.requires_confirmation);
            let unconfirmed = apply(
                &h.manager,
                "t.a".into(),
                RECOVER,
                false,
                plan_a.affected.clone(),
            )
            .await
            .expect("an outcome, not an error");
            assert_eq!(
                unconfirmed,
                ApplyResultDto::ConfirmationRequired {
                    plan: plan_a.clone()
                }
            );

            // A dependent whose dependency failed: not applicable, with the
            // blocking module.
            let blocked = apply(&h.manager, "t.c".into(), ENABLE, true, names(&["t.a"]))
                .await
                .expect("an outcome, not an error");
            let ApplyResultDto::NotApplicable { blockers } = blocked else {
                panic!("expected notApplicable, got {blocked:?}");
            };
            assert!(matches!(
                blockers.as_slice(),
                [BlockerDto { id, state: ModuleStateDto::Failed { .. } }] if id == "t.a"
            ));

            // A stale acknowledged list: the current plan comes back.
            let stale = apply(&h.manager, "t.a".into(), RECOVER, true, Vec::new())
                .await
                .expect("an outcome, not an error");
            assert_eq!(
                stale,
                ApplyResultDto::ImpactChanged {
                    plan: plan_a.clone()
                }
            );
            assert_eq!(plan_a.affected, names(&["t.b"]));
            assert!(h.events().is_empty(), "none of these changed anything");
            for module in ["t.a", "t.b", "t.c"] {
                assert_eq!(
                    count(&h.probe(module).created),
                    if module == "t.c" { 0 } else { 1 }
                );
            }

            // Confirmed with the current list: applied, with the report.
            let applied = apply(&h.manager, "t.a".into(), RECOVER, true, plan_a.affected)
                .await
                .expect("applied");
            let ApplyResultDto::Applied { report } = applied else {
                panic!("expected applied, got {applied:?}");
            };
            assert_eq!(report.started, names(&["t.a", "t.b"]));
            assert!(report.failed.is_empty() && !report.interrupted);
        });
    }

    #[test]
    fn a_plan_shows_steps_and_affected_modules() {
        run(async {
            let h = harness(&[("t.a", &[]), ("t.b", &["t.a"])]).await;
            let enable = plan(&h.manager, "t.b".into(), ENABLE).expect("plan");
            assert_eq!(enable.target, "t.b");
            assert_eq!(enable.action, ENABLE);
            assert_eq!(enable.affected, names(&["t.a"]));
            assert!(enable.requires_confirmation && enable.blockers.is_empty());
            assert_eq!(
                enable.steps,
                [
                    PlanStepDto::Enable { id: "t.a".into() },
                    PlanStepDto::Start { id: "t.a".into() },
                    PlanStepDto::Enable { id: "t.b".into() },
                    PlanStepDto::Start { id: "t.b".into() },
                ]
            );
            // The UI cannot skip the confirmation: Rust checks it.
            let unconfirmed = apply(&h.manager, "t.b".into(), ENABLE, false, Vec::new())
                .await
                .expect("an outcome");
            assert_eq!(
                unconfirmed,
                ApplyResultDto::ConfirmationRequired { plan: enable }
            );
            assert_eq!(count(&h.probe("t.a").created), 0);
        });
    }

    #[test]
    fn real_errors_come_back_as_error_dto() {
        run(async {
            let h = harness(&[("t.a", &[])]).await;
            let m = &h.manager;
            assert_eq!(error_code(get(m, "t.zz".into())), code::UNKNOWN_MODULE);
            assert_eq!(error_code(get(m, "Not An Id".into())), code::INVALID_ID);
            assert_eq!(
                error_code(plan(m, "t.zz".into(), ENABLE)),
                code::UNKNOWN_MODULE
            );
            assert_eq!(
                error_code(plan(m, "t.a".into(), DISABLE)),
                code::ACTION_NOT_APPLICABLE
            );
            assert_eq!(
                error_code(apply(m, "t.a".into(), ENABLE, true, names(&["bad id"])).await),
                code::INVALID_ID
            );
            assert_eq!(
                error_code(apply(m, "t.a".into(), DISABLE, true, Vec::new()).await),
                code::ACTION_NOT_APPLICABLE
            );
            assert_eq!(
                error_code(get_settings(m, "t.zz".into())),
                code::UNKNOWN_MODULE
            );
            assert_eq!(
                error_code(check_health(m, "t.a".into()).await),
                code::NOT_RUNNING
            );

            // Refused settings: the reason, never the value.
            let refused = update_settings(m, "t.a".into(), json!({"password": "hunter2-value"}))
                .await
                .expect_err("refused");
            assert_eq!(refused.code, code::INVALID_SETTINGS);
            assert!(refused.message.contains("password"));
            assert!(!refused.message.contains("hunter2-value"));

            // The store fails: nothing changes, the details stay in the log.
            h.store.fail_saves(true);
            let failed = update_settings(m, "t.a".into(), json!({"level": 1}))
                .await
                .expect_err("store error");
            assert_eq!(failed.code, code::STORE);
            assert!(!failed.message.contains("/secret/path"));
            assert_eq!(
                error_code(apply(m, "t.a".into(), ENABLE, true, Vec::new()).await),
                code::STORE
            );
            let settings = get_settings(m, "t.a".into()).expect("settings");
            assert_eq!(settings.value, json!({}));
            assert!(settings.valid && !settings.restart_required);
            assert_eq!(settings.id, "t.a");
        });
    }

    #[test]
    fn reads_before_ready_return_not_ready_and_commands_after_the_exit_shut_down() {
        run(async {
            let store = TestStore::default();
            store.hang_load("t.a");
            let h = Harness::spawn(&[("t.a", &[])], &[], store.clone());
            tokio::task::yield_now().await;
            let m = &h.manager;
            assert_eq!(error_code(list(m)), code::NOT_READY);
            assert_eq!(error_code(get(m, "t.a".into())), code::NOT_READY);
            assert_eq!(error_code(plan(m, "t.a".into(), ENABLE)), code::NOT_READY);
            assert_eq!(error_code(get_settings(m, "t.a".into())), code::NOT_READY);
        });
        run(async {
            let h = harness(&[("t.a", &[])]).await;
            let m = &h.manager;
            m.shutdown().await.expect("exit");
            assert_eq!(
                error_code(apply(m, "t.a".into(), ENABLE, true, Vec::new()).await),
                code::SHUT_DOWN
            );
            assert_eq!(
                error_code(update_settings(m, "t.a".into(), json!({})).await),
                code::SHUT_DOWN
            );
            assert_eq!(
                error_code(check_health(m, "t.a".into()).await),
                code::SHUT_DOWN
            );
            // The snapshot stays readable after the exit (ADR-015).
            assert_eq!(list(m).expect("list").len(), 1);
        });
    }

    #[test]
    fn an_unreadable_configuration_is_shown_and_its_settings_are_an_error() {
        run(async {
            let store = TestStore::default();
            store.fail_load("t.a");
            store.put("t.b", true, json!({}));
            let h = Harness::spawn(&[("t.a", &[]), ("t.b", &["t.a"])], &[], store.clone());
            h.ready().await;
            let m = &h.manager;
            let listed = list(m).expect("list");
            assert_eq!(listed[0].enabled, None);
            assert!(matches!(
                listed[0].state,
                ModuleStateDto::ConfigUnavailable { .. }
            ));
            assert_eq!(
                listed[1].state,
                ModuleStateDto::Blocked {
                    by: "t.a".into(),
                    reason: BlockReasonDto::DependencyConfigUnavailable
                }
            );
            assert_eq!(
                error_code(get_settings(m, "t.a".into())),
                code::CONFIG_UNAVAILABLE
            );
            assert_eq!(
                error_code(update_settings(m, "t.a".into(), json!({})).await),
                code::CONFIG_UNAVAILABLE
            );
            assert_eq!(
                error_code(plan(m, "t.a".into(), ENABLE)),
                code::ACTION_NOT_APPLICABLE
            );
        });
    }
}
