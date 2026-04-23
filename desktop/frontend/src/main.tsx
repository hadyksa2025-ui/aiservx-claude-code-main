import React from "react";
import ReactDOM from "react-dom/client";
// Self-host the UI fonts so offline installs still get the intended
// typography (audit §7.6). Inter covers UI text, JetBrains Mono
// covers code / identifiers. We only pull weights we actually use
// (400/500/600) to keep the bundle small.
import "@fontsource/inter/400.css";
import "@fontsource/inter/500.css";
import "@fontsource/inter/600.css";
import "@fontsource/jetbrains-mono/400.css";
import "@fontsource/jetbrains-mono/500.css";
import "@fontsource/jetbrains-mono/600.css";
 
// ── TEMP PR-N TEST SCAFFOLD ──────────────────────────────────────────
// Stubs Tauri internals so Vite dev can render the app without a Tauri
// host, and exposes the Zustand store as window.__store so devtools
// console can drive synthetic pushPipelineEvent() calls that are
// indistinguishable from real backend emits. REVERT BEFORE COMMIT.
(window as unknown as { __TAURI_INTERNALS__: unknown }).__TAURI_INTERNALS__ = {
  invoke: async (cmd: string, _args?: unknown) => {
    if (cmd === "get_settings") {
      return {
        openrouter_api_key: "",
        openrouter_model: "openrouter/auto",
        ollama_base_url: "http://localhost:11434",
        ollama_model: "deepseek-coder:6.7b",
        provider_mode: "local",
        planner_model: "",
        reviewer_model: "",
        executor_model: "",
        reviewer_enabled: true,
        max_iterations: 8,
        cmd_confirm_required: true,
        cmd_allow_list: [],
        autonomous_mode: false,
        max_retries_per_task: 3,
        max_total_tasks: 20,
        task_timeout_secs: 180,
        goal_timeout_secs: 3600,
        retry_backoff_base_ms: 1000,
        circuit_breaker_threshold: 5,
        max_parallel_tasks: 1,
        autonomous_confirm_irreversible: false,
        context_compaction_enabled: false,
        context_compaction_keep_last: 20,
      };
    }
    if (cmd === "check_planner" || cmd === "check_executor") return false;
    if (cmd === "get_last_project_dir") return null;
    if (cmd === "load_task_tree") return null;
    if (cmd === "load_failures_log") return [];
    return null;
  },
  transformCallback: (_cb: unknown, _once: boolean) => 0,
};
import { useAppStore } from "./store";
(window as unknown as { __store: unknown }).__store = useAppStore;
// ── END TEMP PR-N TEST SCAFFOLD ──────────────────────────────────────
 
import App from "./App";
import "./styles.css";

ReactDOM.createRoot(document.getElementById("root")!).render(
  <React.StrictMode>
    <App />
  </React.StrictMode>,
);
