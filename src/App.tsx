import { useEffect, useState } from "react";
import "./App.css";
import { loadModulesWhenReady, toIpcError } from "./lib/ipc";
import type { IpcError, Module } from "./types/modules";

type View =
  | { phase: "initializing" }
  | { phase: "ready"; modules: Module[] }
  | { phase: "error"; error: IpcError };

// Minimal status line of the module manager (issue #11). The modules panel
// arrives with #12.
function App() {
  const [view, setView] = useState<View>({ phase: "initializing" });

  useEffect(() => {
    let cancelled = false;
    loadModulesWhenReady()
      .then((modules) => {
        if (!cancelled) setView({ phase: "ready", modules });
      })
      .catch((error: unknown) => {
        if (!cancelled) setView({ phase: "error", error: toIpcError(error) });
      });
    return () => {
      cancelled = true;
    };
  }, []);

  return (
    <main className="container">
      <h1>ATLAS</h1>
      {view.phase === "initializing" && <p>Менеджер модулей: инициализация…</p>}
      {view.phase === "ready" && <p>Менеджер модулей готов. Модулей: {view.modules.length}.</p>}
      {view.phase === "error" && <p>Ошибка: {view.error.message}</p>}
    </main>
  );
}

export default App;
