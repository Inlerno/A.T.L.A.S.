//! Data transfer objects of the IPC layer (issue #11).
//!
//! Everything the webview receives is one of these types, serialized in
//! camelCase and mirrored by hand in `src/types/modules.ts` (a test keeps the
//! two in step). Internal types never cross the IPC boundary directly.

pub mod modules;

use serde::Serialize;

use crate::error::AppError;
use crate::modules::contract::ModuleError;
use crate::modules::manager::ManagerError;

/// Stable machine-readable error codes of [`ErrorDto`].
pub mod code {
    pub const INVALID_ID: &str = "invalid_id";
    pub const UNKNOWN_MODULE: &str = "unknown_module";
    pub const ACTION_NOT_APPLICABLE: &str = "action_not_applicable";
    pub const INVALID_SETTINGS: &str = "invalid_settings";
    pub const NOT_RUNNING: &str = "not_running";
    pub const NOT_READY: &str = "not_ready";
    pub const SHUT_DOWN: &str = "shut_down";
    pub const CONFIG_UNAVAILABLE: &str = "config_unavailable";
    pub const STORE: &str = "store";
    pub const INTERNAL: &str = "internal";
}

/// An error as the UI sees it: a stable `code` to branch on and a fixed,
/// user-safe `message`.
///
/// Only for real errors (an unknown module, a refused value, an internal
/// failure). Expected outcomes of a change — confirmation required, impact
/// changed, not applicable — are not errors: see
/// [`modules::ApplyResultDto`].
///
/// The message never carries the text of the underlying error, paths or
/// settings values: those go to the log only. The one variable part is the
/// reason of `invalid_settings`, which by contract (ADR-015) names the
/// problem and never the value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ErrorDto {
    pub code: &'static str,
    pub message: String,
}

impl ErrorDto {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    /// An error the user can do nothing about. The details are logged here
    /// and never shown.
    pub fn internal(error: &dyn std::error::Error) -> Self {
        tracing::error!(%error, "internal error in an IPC command");
        Self::new(code::INTERNAL, "Внутренняя ошибка приложения")
    }
}

impl From<ModuleError> for ErrorDto {
    fn from(error: ModuleError) -> Self {
        match error {
            ModuleError::InvalidId { .. } => {
                Self::new(code::INVALID_ID, "Некорректный идентификатор модуля")
            }
            ModuleError::UnknownModule(_) => Self::new(code::UNKNOWN_MODULE, "Модуль не найден"),
            ModuleError::ActionNotApplicable { .. } => Self::new(
                code::ACTION_NOT_APPLICABLE,
                "Действие неприменимо к модулю в его текущем состоянии",
            ),
            ModuleError::InvalidSettings { reason, .. } => Self::new(
                code::INVALID_SETTINGS,
                format!("Настройки модуля некорректны: {reason}"),
            ),
            // Catalog errors: a programming error, never caused by a command.
            error @ (ModuleError::DuplicateId(_)
            | ModuleError::MissingDependency { .. }
            | ModuleError::SelfDependency(_)
            | ModuleError::DependencyCycle { .. }) => Self::internal(&error),
        }
    }
}

impl From<ManagerError> for ErrorDto {
    fn from(error: ManagerError) -> Self {
        match error {
            ManagerError::Module(error) => error.into(),
            ManagerError::NotRunning(_) => Self::new(code::NOT_RUNNING, "Модуль не запущен"),
            ManagerError::ShutDown => Self::new(code::SHUT_DOWN, "Приложение завершает работу"),
            ManagerError::NotReady => Self::new(code::NOT_READY, "Менеджер модулей ещё не готов"),
            ManagerError::ConfigUnavailable(_) => Self::new(
                code::CONFIG_UNAVAILABLE,
                "Не удалось прочитать конфигурацию модуля",
            ),
            ManagerError::Store => Self::new(
                code::STORE,
                "Не удалось сохранить конфигурацию; изменения не применены",
            ),
        }
    }
}

impl From<AppError> for ErrorDto {
    fn from(error: AppError) -> Self {
        Self::internal(&error)
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::modules::contract::ModuleId;
    use crate::modules::plan::ChangeAction;

    fn id(value: &str) -> ModuleId {
        ModuleId::new(value).expect("valid module id")
    }

    #[test]
    fn internal_errors_show_neither_the_source_text_nor_paths() {
        let path = PathBuf::from("/secret/path/C:/Users/alice/atlas.log");
        let source = std::io::Error::other(format!("cannot open {}", path.display()));
        let source_text = source.to_string();
        let dto = ErrorDto::from(AppError::Io(source));
        assert_eq!(dto.code, code::INTERNAL);
        assert_eq!(dto.message, "Внутренняя ошибка приложения");
        assert!(!dto.message.contains(&source_text));
        assert!(!dto.message.contains("secret") && !dto.message.contains("alice"));

        for error in [
            ModuleError::DuplicateId(id("secret.module")),
            ModuleError::SelfDependency(id("secret.module")),
            ModuleError::MissingDependency {
                module: id("secret.module"),
                dependency: id("secret.other"),
            },
            ModuleError::DependencyCycle {
                path: vec![id("secret.module"), id("secret.other")],
            },
        ] {
            let text = error.to_string();
            let dto = ErrorDto::from(error);
            assert_eq!(dto.code, code::INTERNAL);
            assert!(!dto.message.contains(&text) && !dto.message.contains("secret"));
        }
    }

    #[test]
    fn every_manager_error_has_a_stable_code_and_a_fixed_message() {
        let cases = [
            (
                ManagerError::Module(ModuleError::InvalidId {
                    id: "Bad Id".into(),
                }),
                code::INVALID_ID,
            ),
            (
                ManagerError::Module(ModuleError::UnknownModule(id("t.zz"))),
                code::UNKNOWN_MODULE,
            ),
            (
                ManagerError::Module(ModuleError::ActionNotApplicable {
                    module: id("t.a"),
                    action: ChangeAction::Enable,
                }),
                code::ACTION_NOT_APPLICABLE,
            ),
            (ManagerError::NotRunning(id("t.a")), code::NOT_RUNNING),
            (ManagerError::ShutDown, code::SHUT_DOWN),
            (ManagerError::NotReady, code::NOT_READY),
            (
                ManagerError::ConfigUnavailable(id("t.a")),
                code::CONFIG_UNAVAILABLE,
            ),
            (ManagerError::Store, code::STORE),
        ];
        for (error, expected) in cases {
            let internal_text = error.to_string();
            let dto = ErrorDto::from(error);
            assert_eq!(dto.code, expected);
            assert!(!dto.message.is_empty());
            assert!(
                !dto.message.contains(&internal_text),
                "{expected}: the internal text is not shown"
            );
            assert!(!dto.message.contains("t.a") && !dto.message.contains("Bad Id"));
        }
    }

    #[test]
    fn invalid_settings_show_the_reason_which_never_holds_the_value() {
        let dto = ErrorDto::from(ManagerError::Module(ModuleError::InvalidSettings {
            module: id("t.a"),
            reason: "ключ `telegram.bot_token` похож на секрет".into(),
        }));
        assert_eq!(dto.code, code::INVALID_SETTINGS);
        assert_eq!(
            dto.message,
            "Настройки модуля некорректны: ключ `telegram.bot_token` похож на секрет"
        );
    }

    #[test]
    fn serializes_as_code_and_message() {
        let json = serde_json::to_value(ErrorDto::from(ManagerError::NotReady)).expect("json");
        assert_eq!(
            json,
            serde_json::json!({ "code": "not_ready", "message": "Менеджер модулей ещё не готов" })
        );
    }
}
