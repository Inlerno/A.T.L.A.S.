// Modules panel (issue #12): the list of modules with their state, and the
// actions on them. Everything shown comes from the Rust core; every change
// goes through it and is confirmed by the user where the core asks for it.

import { useState } from "react";

import { applyModuleChange, checkModuleHealth, toIpcError } from "../../lib/ipc";
import type { ApplyReport, ChangeAction, Module, ModuleId } from "../../types/modules";
import { type ChangeDialog, ConfirmChangeDialog } from "./ConfirmChangeDialog";
import { healthLabel, nameOf, stateDetails, stateLabel } from "./labels";
import "./modules.css";
import { SettingsEditor } from "./SettingsEditor";
import { useModules } from "./useModules";

type Notice = { kind: "error" | "info"; text: string };

/** What the user is told after a change was applied. `null` — nothing to add. */
function reportNotice(report: ApplyReport, modules: Module[]): Notice | null {
  const parts: string[] = [];
  for (const failed of report.failed) {
    parts.push(`${nameOf(failed.id, modules)}: ${failed.error}`);
  }
  if (report.blocked.length > 0) {
    const names = report.blocked.map((id) => nameOf(id, modules)).join(", ");
    parts.push(`Заблокированы: ${names}.`);
  }
  if (report.interrupted) {
    parts.push("Действие прервано: приложение завершает работу.");
  }
  return parts.length > 0 ? { kind: "error", text: parts.join(" ") } : null;
}

export function ModulesPanel() {
  const view = useModules();
  const [dialog, setDialog] = useState<ChangeDialog | null>(null);
  const [busy, setBusy] = useState(false);
  const [notice, setNotice] = useState<Notice | null>(null);
  const [openSettings, setOpenSettings] = useState<ModuleId | null>(null);

  if (view.phase === "initializing") {
    return (
      <p className="panel-status" role="status">
        Менеджер модулей: инициализация…
      </p>
    );
  }
  if (view.phase === "error") {
    return (
      <p className="notice notice-error" role="alert">
        Не удалось получить список модулей: {view.error.message}
      </p>
    );
  }
  const { modules } = view;

  // One path for every change: the core decides whether a confirmation is
  // needed, whether the plan the user saw is still current, and whether the
  // change is possible at all.
  const change = async (
    id: ModuleId,
    action: ChangeAction,
    confirmed: boolean,
    acknowledged: ModuleId[],
  ) => {
    setBusy(true);
    setNotice(null);
    try {
      const result = await applyModuleChange(id, action, confirmed, acknowledged);
      switch (result.outcome) {
        case "applied":
          setDialog(null);
          setNotice(reportNotice(result.report, modules));
          break;
        case "confirmationRequired":
          setDialog({ kind: "confirm", plan: result.plan, impactChanged: false });
          break;
        case "impactChanged":
          setDialog({ kind: "confirm", plan: result.plan, impactChanged: true });
          break;
        case "notApplicable":
          setDialog({ kind: "notApplicable", id, action, blockers: result.blockers });
          break;
      }
    } catch (error) {
      setDialog(null);
      setNotice({ kind: "error", text: toIpcError(error).message });
    } finally {
      setBusy(false);
    }
  };

  const check = async (id: ModuleId) => {
    setBusy(true);
    setNotice(null);
    try {
      // The result also arrives as an event and is shown in the list.
      await checkModuleHealth(id);
    } catch (error) {
      setNotice({ kind: "error", text: toIpcError(error).message });
    } finally {
      setBusy(false);
    }
  };

  return (
    <section className="modules" aria-label="Модули">
      {notice && (
        <p className={`notice notice-${notice.kind}`} role="alert">
          {notice.text}
        </p>
      )}

      {modules.length === 0 ? (
        <p className="panel-status">Модули не зарегистрированы.</p>
      ) : (
        <ul className="module-list">
          {modules.map((module) => {
            const { id, displayName, description, version, dependencies } = module.descriptor;
            const { state, enabled } = module;
            const details = stateDetails(state, modules);
            const running = state.kind === "running";
            const recoverable = state.kind === "failed" || state.kind === "blocked";
            return (
              <li key={id} className={`module module-${state.kind}`}>
                <div className="module-head">
                  <label className="switch">
                    <input
                      type="checkbox"
                      role="switch"
                      checked={enabled === true}
                      disabled={busy || enabled === null}
                      onChange={() => void change(id, enabled ? "disable" : "enable", false, [])}
                    />
                    <span>
                      {enabled === null ? "неизвестно" : enabled ? "включён" : "отключён"}
                    </span>
                  </label>
                  <div className="module-title">
                    <strong>{displayName}</strong>
                    <code>{id}</code>
                    <span className="module-version">v{version}</span>
                  </div>
                  <span className={`state state-${state.kind}`}>{stateLabel(state)}</span>
                </div>

                <p className="module-description">{description}</p>
                {dependencies.length > 0 && (
                  <p className="module-meta">
                    Зависит от: {dependencies.map((dep) => nameOf(dep, modules)).join(", ")}
                  </p>
                )}
                {details && <p className="module-details">{details}</p>}
                {running && <p className="module-meta">Состояние: {healthLabel(module.health)}</p>}
                {module.restartRequired && (
                  <p className="notice notice-warning">
                    Настройки изменены: нужен перезапуск модуля.
                  </p>
                )}

                <div className="module-actions">
                  {recoverable && (
                    <button
                      type="button"
                      className="primary"
                      disabled={busy}
                      onClick={() => void change(id, "recover", false, [])}
                    >
                      Восстановить
                    </button>
                  )}
                  {running && (
                    <button
                      type="button"
                      className={module.restartRequired ? "primary" : undefined}
                      disabled={busy}
                      onClick={() => void change(id, "restart", false, [])}
                    >
                      Перезапустить
                    </button>
                  )}
                  {running && (
                    <button type="button" disabled={busy} onClick={() => void check(id)}>
                      Проверить
                    </button>
                  )}
                  <button
                    type="button"
                    aria-expanded={openSettings === id}
                    onClick={() => setOpenSettings(openSettings === id ? null : id)}
                  >
                    {openSettings === id ? "Скрыть настройки" : "Настройки"}
                  </button>
                </div>

                {openSettings === id && <SettingsEditor id={id} />}
              </li>
            );
          })}
        </ul>
      )}

      {dialog && (
        <ConfirmChangeDialog
          dialog={dialog}
          modules={modules}
          busy={busy}
          onConfirm={(plan) => void change(plan.target, plan.action, true, plan.affected)}
          onCancel={() => setDialog(null)}
        />
      )}
    </section>
  );
}
