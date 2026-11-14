// State of the modules panel (issue #12): loaded once the manager is ready
// and then kept current by events only — there is no polling.

import { useEffect, useState } from "react";

import {
  getModule,
  listModules,
  loadModulesWhenReady,
  onModuleEvent,
  onModulesResync,
  toIpcError,
} from "../../lib/ipc";
import type { IpcError, Module, ModuleId, UiEvent } from "../../types/modules";

export type ModulesView =
  /** The manager is not ready: there is no list yet — not an empty one. */
  | { phase: "initializing" }
  | { phase: "ready"; modules: Module[] }
  | { phase: "error"; error: IpcError };

/** What an event says about its module, applied before the module is read again. */
function applyEvent(module: Module, { event }: UiEvent): Module {
  switch (event.type) {
    case "statusChanged":
      return {
        ...module,
        enabled: event.current.enabled,
        state: event.current.state,
        // Health belongs to the running instance.
        health: event.current.state.kind === "running" ? module.health : null,
      };
    case "healthChanged":
      return { ...module, health: event.health };
    case "settingsChanged":
      return { ...module, restartRequired: event.restartRequired };
    case "managerReady":
      return module;
  }
}

export function useModules(): ModulesView {
  const [view, setView] = useState<ModulesView>({ phase: "initializing" });

  useEffect(() => {
    let disposed = false;
    // `true` while the whole list is being read; an event that arrives then
    // may be newer than the answer, so the list is read once more.
    let loading = true;
    let stale = false;
    // Per module: number of its last request. Only the answer to the last one
    // is used, so an older answer never replaces a newer one.
    const requests = new Map<ModuleId, number>();
    const unlisten: (() => void)[] = [];

    const patch = (id: ModuleId, change: (module: Module) => Module) => {
      setView((current) =>
        current.phase === "ready"
          ? {
              phase: "ready",
              modules: current.modules.map((module) =>
                module.descriptor.id === id ? change(module) : module,
              ),
            }
          : current,
      );
    };

    const fail = (error: unknown) => {
      if (!disposed) setView({ phase: "error", error: toIpcError(error) });
    };

    // Reads the module again: an event does not carry everything (for
    // example, a restart clears `restartRequired` without an event of its own).
    const refresh = (id: ModuleId) => {
      const request = (requests.get(id) ?? 0) + 1;
      requests.set(id, request);
      getModule(id)
        .then((module) => {
          if (!disposed && requests.get(id) === request) patch(id, () => module);
        })
        // The event itself is already applied; the next event or resync
        // reads the module again.
        .catch(() => {});
    };

    const reload = async (first: () => Promise<Module[]>) => {
      loading = true;
      try {
        let read = first;
        do {
          stale = false;
          const modules = await read();
          if (disposed) return;
          requests.clear();
          setView({ phase: "ready", modules });
          read = listModules;
        } while (stale);
      } catch (error) {
        fail(error);
      } finally {
        loading = false;
      }
    };

    const onEvent = (message: UiEvent) => {
      const { event } = message;
      if (event.type === "managerReady") return;
      if (loading) {
        stale = true;
        return;
      }
      patch(event.id, (module) => applyEvent(module, message));
      refresh(event.id);
    };

    const onResync = () => {
      if (loading) {
        stale = true;
      } else {
        void reload(listModules);
      }
    };

    // Subscribe first, load second: nothing published in between is lost.
    Promise.all([onModuleEvent(onEvent), onModulesResync(onResync)])
      .then((subscriptions) => {
        if (disposed) {
          subscriptions.forEach((stop) => stop());
          return;
        }
        unlisten.push(...subscriptions);
        void reload(loadModulesWhenReady);
      })
      .catch(fail);

    return () => {
      disposed = true;
      unlisten.forEach((stop) => stop());
    };
  }, []);

  return view;
}
