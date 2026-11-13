// The only place that talks to the Rust core (issue #11): every `invoke` and
// `listen` of the application lives here, typed with `src/types/modules.ts`.
//
// Commands reject with an `IpcError` for real errors. Expected outcomes of a
// change (confirmation required, impact changed, not applicable) are values
// of `ApplyResult`, not errors.

import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";

import type {
  ApplyResult,
  ChangeAction,
  ChangePlan,
  Health,
  IpcError,
  Module,
  ModuleId,
  ModuleSettings,
  Resync,
  UiEvent,
} from "../types/modules";

const MODULE_EVENT = "modules://event";
const MODULES_RESYNC = "modules://resync";

/** Turns whatever a command rejected with into an `IpcError`. */
export function toIpcError(error: unknown): IpcError {
  if (
    typeof error === "object" &&
    error !== null &&
    "code" in error &&
    "message" in error &&
    typeof error.code === "string" &&
    typeof error.message === "string"
  ) {
    return error as IpcError;
  }
  // Not an `ErrorDto`: a failure of the IPC itself. Its text is not shown.
  return { code: "internal", message: "Внутренняя ошибка приложения" };
}

/** All modules in start order. Rejects with `not_ready` until the manager is ready. */
export function listModules(): Promise<Module[]> {
  return invoke<Module[]>("list_modules");
}

export function getModule(id: ModuleId): Promise<Module> {
  return invoke<Module>("get_module", { id });
}

/** The plan to show the user before confirmation. Changes nothing. */
export function planModuleChange(id: ModuleId, action: ChangeAction): Promise<ChangePlan> {
  return invoke<ChangePlan>("plan_module_change", { id, action });
}

/**
 * Applies a change. `acknowledged` is the `affected` list of the plan the user
 * saw. The core checks the confirmation again; `recover` always needs
 * `confirmed`.
 */
export function applyModuleChange(
  id: ModuleId,
  action: ChangeAction,
  confirmed: boolean,
  acknowledged: ModuleId[],
): Promise<ApplyResult> {
  return invoke<ApplyResult>("apply_module_change", { id, action, confirmed, acknowledged });
}

/** Informational check of a running module; it never changes the status. */
export function checkModuleHealth(id: ModuleId): Promise<Health> {
  return invoke<Health>("check_module_health", { id });
}

export function getModuleSettings(id: ModuleId): Promise<ModuleSettings> {
  return invoke<ModuleSettings>("get_module_settings", { id });
}

/** Saves new settings. A running module is not restarted: see `restartRequired`. */
export function updateModuleSettings(
  id: ModuleId,
  settings: Record<string, unknown>,
): Promise<void> {
  return invoke<void>("update_module_settings", { id, settings });
}

/** Subscribes to module events. Resolves to the function that unsubscribes. */
export function onModuleEvent(handler: (event: UiEvent) => void): Promise<UnlistenFn> {
  return listen<UiEvent>(MODULE_EVENT, (message) => handler(message.payload));
}

/** Called when events were missed: load the modules again. */
export function onModulesResync(handler: (resync: Resync) => void): Promise<UnlistenFn> {
  return listen<Resync>(MODULES_RESYNC, (message) => handler(message.payload));
}

/**
 * Loads the modules once the manager is ready, without polling.
 *
 * Subscribes first and asks second, so `managerReady` cannot be missed: if it
 * was published before the subscription, the request already succeeds;
 * otherwise the request is rejected with `not_ready` and repeated when the
 * event arrives. Until this resolves there is no list — not an empty one.
 */
export async function loadModulesWhenReady(): Promise<Module[]> {
  let announce: () => void = () => {};
  const ready = new Promise<void>((resolve) => {
    announce = resolve;
  });
  const unlisten = await onModuleEvent((message) => {
    if (message.event.type === "managerReady") {
      announce();
    }
  });
  try {
    try {
      return await listModules();
    } catch (error) {
      if (toIpcError(error).code !== "not_ready") {
        throw error;
      }
    }
    await ready;
    return await listModules();
  } finally {
    unlisten();
  }
}
