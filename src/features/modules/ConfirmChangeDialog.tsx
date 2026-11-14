// Dialog of a module change (issue #12): the confirmation with the affected
// modules, or the explanation why the change is not possible now.

import { useEffect, useRef } from "react";

import type { Blocker, ChangeAction, ChangePlan, Module, ModuleId } from "../../types/modules";
import { actionLabel, affectedLabel, blockerLabel, nameOf, stepLabel } from "./labels";

export type ChangeDialog =
  /** The core asked for a confirmation of `plan`. */
  | { kind: "confirm"; plan: ChangePlan; impactChanged: boolean }
  /** The core refused: `blockers` stand in the way. */
  | { kind: "notApplicable"; id: ModuleId; action: ChangeAction; blockers: Blocker[] };

interface Props {
  dialog: ChangeDialog;
  modules: Module[];
  /** A change is being applied: nothing can be pressed. */
  busy: boolean;
  /** The user confirmed exactly this plan. */
  onConfirm: (plan: ChangePlan) => void;
  /** Closes the dialog; nothing is changed. */
  onCancel: () => void;
}

export function ConfirmChangeDialog({ dialog, modules, busy, onConfirm, onCancel }: Props) {
  const element = useRef<HTMLDialogElement>(null);

  useEffect(() => {
    const current = element.current;
    if (current && !current.open) current.showModal();
    return () => current?.close();
  }, []);

  const target = dialog.kind === "confirm" ? dialog.plan.target : dialog.id;
  const action = dialog.kind === "confirm" ? dialog.plan.action : dialog.action;
  const title = `${actionLabel(action)}: ${nameOf(target, modules)}`;

  return (
    <dialog
      ref={element}
      className="change-dialog"
      aria-labelledby="change-dialog-title"
      onCancel={(event) => {
        // Esc: the same as «Отмена», and not while a change is applied.
        event.preventDefault();
        if (!busy) onCancel();
      }}
    >
      <h2 id="change-dialog-title">{title}</h2>

      {dialog.kind === "confirm" ? (
        <>
          {dialog.impactChanged && (
            <p className="notice notice-warning">
              Пока вы принимали решение, состояние модулей изменилось. Проверьте новый список.
            </p>
          )}
          {dialog.plan.affected.length > 0 ? (
            <>
              <p>{affectedLabel(action)}</p>
              <ul className="affected">
                {dialog.plan.affected.map((id) => (
                  <li key={id}>{nameOf(id, modules)}</li>
                ))}
              </ul>
            </>
          ) : (
            <p>Другие модули не затрагиваются.</p>
          )}
          {dialog.plan.steps.length > 0 && (
            <>
              <p>Порядок действий:</p>
              <ol className="steps">
                {dialog.plan.steps.map((step) => (
                  <li key={`${step.kind}:${step.id}`}>{stepLabel(step, modules)}</li>
                ))}
              </ol>
            </>
          )}
          <div className="dialog-actions">
            <button type="button" onClick={onCancel} disabled={busy} autoFocus>
              Отмена
            </button>
            <button
              type="button"
              className="primary"
              onClick={() => onConfirm(dialog.plan)}
              disabled={busy}
            >
              {busy ? "Выполняется…" : actionLabel(action)}
            </button>
          </div>
        </>
      ) : (
        <>
          <p>Сейчас это действие выполнить нельзя.</p>
          <ul className="affected">
            {dialog.blockers.map((blocker) => (
              <li key={blocker.id}>{blockerLabel(blocker, modules)}</li>
            ))}
          </ul>
          <div className="dialog-actions">
            <button type="button" onClick={onCancel} autoFocus>
              Закрыть
            </button>
          </div>
        </>
      )}
    </dialog>
  );
}
