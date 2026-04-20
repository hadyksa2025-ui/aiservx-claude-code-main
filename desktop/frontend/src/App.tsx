import { useCallback, useEffect, useMemo } from "react";
import { open as openDialog } from "@tauri-apps/plugin-dialog";
import { api, onEvent } from "./api";
import type {
  AgentRole,
  FsChange,
  StepEvent,
  ToolCall,
  ToolResult,
} from "./types";
import { Explorer } from "./components/Explorer";
import { Chat } from "./components/Chat";
import { Execution } from "./components/Execution";
import { SettingsModal } from "./components/Settings";
import { ConfirmCmdOverlay } from "./components/ConfirmCmd";
import { TaskPanel } from "./components/TaskPanel";
import { EVENTS_CAP, useAppStore } from "./store";

export default function App() {
  // Consume store slices by selector so components that only care
  // about (say) health-probe state don't re-render when the event log
  // changes. Audit §7.6.
  const projectDir = useAppStore((s) => s.projectDir);
  const setProjectDir = useAppStore((s) => s.setProjectDir);
  const fsTick = useAppStore((s) => s.fsTick);
  const bumpFsTick = useAppStore((s) => s.bumpFsTick);
  const plannerOk = useAppStore((s) => s.plannerOk);
  const setPlannerOk = useAppStore((s) => s.setPlannerOk);
  const executorOk = useAppStore((s) => s.executorOk);
  const setExecutorOk = useAppStore((s) => s.setExecutorOk);
  const events = useAppStore((s) => s.events);
  const pushEvent = useAppStore((s) => s.pushEvent);
  const pushError = useAppStore((s) => s.pushError);
  const replaceEvents = useAppStore((s) => s.replaceEvents);
  const clearEvents = useAppStore((s) => s.clearEvents);
  const resetMessages = useAppStore((s) => s.resetMessages);
  const debugOpen = useAppStore((s) => s.debugOpen);
  const toggleDebug = useAppStore((s) => s.toggleDebug);
  const explorerOpen = useAppStore((s) => s.explorerOpen);
  const toggleExplorer = useAppStore((s) => s.toggleExplorer);
  const settingsOpen = useAppStore((s) => s.settingsOpen);
  const setSettingsOpen = useAppStore((s) => s.setSettingsOpen);

  // Health check on mount and when settings close
  const refreshHealth = useCallback(async () => {
    try {
      setPlannerOk(await api.checkPlanner());
    } catch {
      setPlannerOk(false);
    }
    try {
      setExecutorOk(await api.checkExecutor());
    } catch {
      setExecutorOk(false);
    }
  }, [setPlannerOk, setExecutorOk]);

  useEffect(() => {
    void refreshHealth();
  }, [refreshHealth]);

  // Global subscription to execution / fs events.
  useEffect(() => {
    const unlistens: Array<Promise<() => void>> = [];
    unlistens.push(
      onEvent<ToolCall>("ai:tool_call", (p) =>
        pushEvent({ kind: "tool_call", call: p, at: Date.now() }),
      ),
    );
    unlistens.push(
      onEvent<ToolResult>("ai:tool_result", (p) =>
        pushEvent({ kind: "tool_result", result: p, at: Date.now() }),
      ),
    );
    unlistens.push(
      onEvent<StepEvent>("ai:step", (p) =>
        pushEvent({ kind: "step", step: p, at: Date.now() }),
      ),
    );
    unlistens.push(
      onEvent<{ message: string; role?: AgentRole }>("ai:error", (p) =>
        pushError(p.message, p.role),
      ),
    );
    unlistens.push(onEvent<FsChange>("fs:changed", () => bumpFsTick()));
    return () => {
      for (const p of unlistens) {
        void p.then((fn) => fn());
      }
    };
  }, [pushEvent, pushError, bumpFsTick]);

  // Watcher lifecycle tied to the opened project.
  useEffect(() => {
    if (!projectDir) return;
    let cancelled = false;
    void (async () => {
      try {
        await api.watchDir(projectDir);
      } catch (e) {
        pushError(`Failed to start watcher: ${String(e)}`);
      }
    })();
    return () => {
      if (cancelled) return;
      cancelled = true;
      void api.unwatchDir(projectDir).catch(() => {});
    };
  }, [projectDir, pushError]);

  const openProject = useCallback(async () => {
    const selected = await openDialog({
      multiple: false,
      directory: true,
      title: "Open project",
    });
    if (typeof selected === "string" && selected.length > 0) {
      setProjectDir(selected);
      resetMessages();
      replaceEvents([
        {
          kind: "info",
          text: `Opened project: ${selected}`,
          at: Date.now(),
        },
      ]);
    }
  }, [setProjectDir, resetMessages, replaceEvents]);

  const statusPlanner = useMemo(() => {
    if (plannerOk === null) return "checking…";
    return plannerOk ? "planner ready" : "planner off";
  }, [plannerOk]);

  const statusExecutor = useMemo(() => {
    if (executorOk === null) return "checking…";
    return executorOk ? "ollama online" : "ollama offline";
  }, [executorOk]);

  const chatDisabled = !projectDir || executorOk === false;

  return (
    <div className="app">
      <header className="topbar" role="banner">
        <h1>Open Claude Code</h1>
        <button onClick={openProject}>Open project…</button>
        <span className="project-path" title={projectDir ?? undefined}>
          {projectDir ?? "no project"}
        </span>
        <div className="spacer" />
        <span
          className="status-badge"
          role="status"
          aria-live="polite"
          title={statusPlanner}
        >
          <span
            className={
              "status-dot " +
              (plannerOk === null ? "" : plannerOk ? "ok" : "bad")
            }
            aria-hidden
          />
          {statusPlanner}
        </span>
        <span
          className="status-badge"
          role="status"
          aria-live="polite"
          title={statusExecutor}
        >
          <span
            className={
              "status-dot " +
              (executorOk === null ? "" : executorOk ? "ok" : "bad")
            }
            aria-hidden
          />
          {statusExecutor}
        </span>
        <button onClick={() => setSettingsOpen(true)} aria-label="Open settings">
          Settings
        </button>
      </header>

      <div
        className={`panes panes-4${
          explorerOpen ? "" : " panes-explorer-collapsed"
        }${debugOpen ? "" : " panes-debug-collapsed"}`}
      >
        <section
          className={`pane pane-explorer${
            explorerOpen ? "" : " pane-collapsed"
          }`}
          aria-label="Explorer"
        >
          <div className="pane-header">
            <button
              className="pane-toggle"
              onClick={toggleExplorer}
              aria-expanded={explorerOpen}
              aria-label={
                explorerOpen ? "collapse explorer" : "expand explorer"
              }
              title={explorerOpen ? "Collapse" : "Expand"}
              type="button"
            >
              <span className="pane-caret" aria-hidden>
                {explorerOpen ? "▾" : "▸"}
              </span>
              Explorer
            </button>
          </div>
          {explorerOpen && (
            <div className="pane-body">
              {projectDir ? (
                <Explorer
                  key={projectDir + ":" + fsTick}
                  projectDir={projectDir}
                />
              ) : (
                <div className="empty-state">
                  Open a project folder to see its files.
                </div>
              )}
            </div>
          )}
        </section>

        <section className="pane" aria-label="Goal and tasks">
          <div className="pane-header">Goal &amp; Tasks</div>
          <div className="pane-body">
            <TaskPanel projectDir={projectDir} disabled={chatDisabled} />
          </div>
        </section>

        <section className="pane" aria-label="Chat">
          <div className="pane-header">Chat</div>
          <div className="pane-body chat">
            <Chat projectDir={projectDir} disabled={chatDisabled} />
          </div>
        </section>

        <section
          className={`pane pane-debug${debugOpen ? "" : " pane-collapsed"}`}
          aria-label="Debug"
        >
          <div className="pane-header">
            <button
              className="pane-toggle"
              onClick={toggleDebug}
              aria-expanded={debugOpen}
              aria-label={debugOpen ? "collapse debug panel" : "expand debug panel"}
              title={debugOpen ? "Collapse" : "Expand"}
              type="button"
            >
              <span className="pane-caret" aria-hidden>
                {debugOpen ? "▾" : "▸"}
              </span>
              Debug
            </button>
            {events.length > 0 && (
              <span
                className="pane-header-count"
                aria-label={`${events.length} event${
                  events.length === 1 ? "" : "s"
                }`}
              >
                {events.length}
                {events.length >= EVENTS_CAP && "+"}
              </span>
            )}
            <div style={{ flex: 1 }} />
            {debugOpen && (
              <button
                onClick={clearEvents}
                style={{ fontSize: 10, padding: "2px 6px" }}
              >
                clear
              </button>
            )}
          </div>
          {debugOpen && (
            <div className="pane-body">
              <Execution events={events} />
            </div>
          )}
        </section>
      </div>

      {settingsOpen && (
        <SettingsModal
          onClose={() => {
            setSettingsOpen(false);
            void refreshHealth();
          }}
        />
      )}
      <ConfirmCmdOverlay />
    </div>
  );
}
