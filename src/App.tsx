import "./App.css";
import { ModulesPanel } from "./features/modules/ModulesPanel";

// The application window so far is the modules panel (issue #12). The Orb and
// the chat arrive with later stages.
function App() {
  return (
    <main className="app">
      <h1>ATLAS — модули</h1>
      <ModulesPanel />
    </main>
  );
}

export default App;
