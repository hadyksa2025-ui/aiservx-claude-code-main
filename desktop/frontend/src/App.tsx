import { useCallback, useEffect, useMemo, useState } from "react";
import { open as openDialog } from "@tauri-apps/plugin-dialog";
import { api, onEvent } from "./api";
import type {
  AgentRole,
  FsChange,
  PipelineStepEvent,
  StepEvent,
  ToolCall,
  ToolResult,
} from "./types";
import { isPipelineRole } from "./types";
import { Explorer } from "./components/Explorer";
import { Chat } from "./components/Chat";
import { Execution } from "./components/Execution";
import { SettingsModal } from "./components/Settings";
import { ConfirmCmdOverlay } from "./components/ConfirmCmd";
import { TaskPanel } from "./components/TaskPanel";
import { TerminalManager } from "./components/TerminalManager";
import { EVENTS_CAP, useAppStore } from "./store";
import type { FailureLogEntry, TaskFailureLoggedEvent } from "./types";

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
  const failures = useAppStore((s) => s.failures);
  const setFailures = useAppStore((s) => s.setFailures);
  const pushFailure = useAppStore((s) => s.pushFailure);
  const clearFailures = useAppStore((s) => s.clearFailures);
  const resetMessages = useAppStore((s) => s.resetMessages);
  const debugOpen = useAppStore((s) => s.debugOpen);
  const toggleDebug = useAppStore((s) => s.toggleDebug);
  const bottomPanelHeight = useAppStore((s) => s.bottomPanelHeight);
  const setBottomPanelHeight = useAppStore((s) => s.setBottomPanelHeight);
  const explorerOpen = useAppStore((s) => s.explorerOpen);
  const toggleExplorer = useAppStore((s) => s.toggleExplorer);
  const settingsOpen = useAppStore((s) => s.settingsOpen);
  const setSettingsOpen = useAppStore((s) => s.setSettingsOpen);
  const setGoalPlanning = useAppStore((s) => s.setGoalPlanning);
  const pushPipelineEvent = useAppStore((s) => s.pushPipelineEvent);

  const [bottomTab, setBottomTab] = useState<"debug" | "terminal" | "failures">(
    "terminal",
  );

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
    // OC-Titan §VI.2/§VI.3 — pipeline events arrive on the same
    // `ai:step` channel but carry a distinct payload shape
    // (`{role, label, status, ...}`). We discriminate by role
    // membership + presence of a string `label`; everything else
    // keeps flowing into the legacy execution log above.
    unlistens.push(
      onEvent<Record<string, unknown>>("ai:step", (raw) => {
        if (!raw || typeof raw !== "object") return;
        const role = (raw as { role?: unknown }).role;
        const label = (raw as { label?: unknown }).label;
        const status = (raw as { status?: unknown }).status;
        if (!isPipelineRole(role)) return;
        if (typeof label !== "string") return;
        if (
          status !== "running" &&
          status !== "done" &&
          status !== "failed" &&
          status !== "warning"
        ) {
          return;
        }
        pushPipelineEvent(raw as unknown as PipelineStepEvent);
      }),
    );
    unlistens.push(
      onEvent<{ message: string; role?: AgentRole }>("ai:error", (p) =>
        pushError(p.message, p.role),
      ),
    );
    unlistens.push(onEvent<FsChange>("fs:changed", () => bumpFsTick()));
    unlistens.push(
      onEvent<TaskFailureLoggedEvent>("task:failure_logged", (p) => {
        pushFailure({
          at: Math.floor(Date.now() / 1000),
          task_id: p.task_id,
          error: p.error,
        });
      }),
    );
    // Scenario-A §9.2 F-2: show a "planning…" chip in TaskPanel from
    // the moment a goal run starts. `goal:planning` fires at scan start
    // and again when the planner stream begins; `goal:planning_done`
    // fires right before the task tree is populated (or on failure).
    unlistens.push(
      onEvent<{ phase: "scanning" | "planning" }>("goal:planning", (p) =>
        setGoalPlanning(p.phase),
      ),
    );
    unlistens.push(
      onEvent<unknown>("goal:planning_done", () => setGoalPlanning(null)),
    );
    return () => {
      for (const p of unlistens) {
        void p.then((fn) => fn());
      }
    };
  }, [
    pushEvent,
    pushError,
    pushFailure,
    bumpFsTick,
    setGoalPlanning,
    pushPipelineEvent,
  ]);

  // Load (and scope) failures per project. We also clear local failures
  // on project close.
  useEffect(() => {
    if (!projectDir) {
      clearFailures();
      return;
    }
    void api
      .loadFailuresLog(projectDir)
      .then((log) => {
        if (Array.isArray(log)) setFailures(log);
      })
      .catch(() => {});
  }, [projectDir, setFailures, clearFailures]);

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
      // Drop the previous project's in-memory failures slice. We do NOT
      // clear the newly-opened project's on-disk `failures_log` here —
      // see PROJECT_MEMORY.md §10.1 A-2. That log is already project-
      // scoped (it lives in that project's PROJECT_MEMORY.json), and
      // wiping it on open would silently destroy the very history the
      // Failures panel is meant to surface. The `useEffect` tied to
      // `projectDir` loads the persisted log into the store via
      // `setFailures`, which atomically replaces the in-memory slice.
      clearFailures();
      setProjectDir(selected);
      resetMessages();
      replaceEvents([
        {
          kind: "info",
          text: `Opened project: ${selected}`,
          at: Date.now(),
        },
      ]);
      // Scenario-A §9.2 F-8: remember this as the last-opened project
      // so a subsequent boot can auto-restore it. Failure to persist is
      // non-fatal — just log it, don't break the open flow.
      api.setLastProjectDir(selected).catch((e) => {
        pushError(`Failed to persist last project: ${String(e)}`);
      });
    }
  }, [setProjectDir, resetMessages, replaceEvents, pushError, clearFailures]);

  // Scenario-A §9.2 F-8: on boot, auto-restore the last-opened project
  // if we have one and it still exists on disk. A single `list_dir` on
  // the root doubles as an existence probe — if the dir is gone we
  // silently skip restore rather than booting into a broken project.
  useEffect(() => {
    let cancelled = false;
    void (async () => {
      try {
        const last = await api.getLastProjectDir();
        if (cancelled || !last) return;
        try {
          await api.listDir(last, "");
        } catch {
          return; // directory no longer readable — skip restore
        }
        if (cancelled) return;
        setProjectDir(last);
        // No clear_failures_log on restore either (see §10.1 A-2).
        // `clearFailures` drops only the transient in-memory slice; the
        // persisted failures_log for `last` is then replayed into the
        // store by the load effect above.
        clearFailures();
        replaceEvents([
          {
            kind: "info",
            text: `Restored last project: ${last}`,
            at: Date.now(),
          },
        ]);
      } catch {
        // ignore — first-run boot has no last project yet
      }
    })();
    return () => {
      cancelled = true;
    };
    // Intentionally empty deps — boot-time only. The store setters we
    // call are stable identities from Zustand.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const statusPlanner = useMemo(() => {
    if (plannerOk === null) return "checking…";
    return plannerOk ? "planner ready" : "planner off";
  }, [plannerOk]);

  // Label is provider-neutral ("executor ready / off") rather than the
  // old "ollama online / offline", because the executor role can resolve
  // to either OpenRouter (Cloud) or Ollama (Local/Hybrid) depending on
  // the configured ProviderMode. The resolved provider is visible in
  // Settings; the badge just reports reachability. Scenario-A §9.2 F-1.
  const statusExecutor = useMemo(() => {
    if (executorOk === null) return "checking…";
    return executorOk ? "executor ready" : "executor off";
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
        className="layout"
        style={{
          gridTemplateRows: debugOpen
            ? `1fr ${bottomPanelHeight}px`
            : "1fr 36px",
        }}
      >
        <div
          className={`panes panes-3${explorerOpen ? "" : " panes-explorer-collapsed"}`}
        >
          <section
            className={`pane pane-explorer${explorerOpen ? "" : " pane-collapsed"}`}
            aria-label="Explorer"
          >
            <div className="pane-header">
              <button
                className="pane-toggle"
                onClick={toggleExplorer}
                aria-expanded={explorerOpen}
                aria-label={explorerOpen ? "collapse explorer" : "expand explorer"}
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
                  <Explorer key={projectDir + ":" + fsTick} projectDir={projectDir} />
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
        </div>

        <section
          className={`pane bottom-panel${debugOpen ? "" : " bottom-collapsed"}`}
          style={debugOpen ? { height: bottomPanelHeight } : undefined}
          aria-label={
            bottomTab === "debug"
              ? "Debug"
              : bottomTab === "terminal"
                ? "Terminal"
                : "Failures"
          }
        >
          {debugOpen && (
            <div
              className="bottom-panel-resize-handle"
              onMouseDown={(e) => {
                e.preventDefault();
                const startY = e.clientY;
                const startHeight = bottomPanelHeight;
                const onMove = (moveEvent: MouseEvent) => {
                  const delta = startY - moveEvent.clientY;
                  const newHeight = Math.max(100, Math.min(600, startHeight + delta));
                  setBottomPanelHeight(newHeight);
                };
                const onUp = () => {
                  window.removeEventListener("mousemove", onMove);
                  window.removeEventListener("mouseup", onUp);
                };
                window.addEventListener("mousemove", onMove);
                window.addEventListener("mouseup", onUp);
              }}
            />
          )}
          <div className="pane-header bottom-header">
            <button
              className="pane-toggle"
              onClick={toggleDebug}
              aria-expanded={debugOpen}
              aria-label={debugOpen ? "collapse bottom panel" : "expand bottom panel"}
              title={debugOpen ? "Collapse" : "Expand"}
              type="button"
            >
              <span className="pane-caret" aria-hidden>
                {debugOpen ? "▾" : "▸"}
              </span>
              Panel
            </button>

            <button
              className={
                "bottom-tab" + (bottomTab === "debug" ? " bottom-tab-active" : "")
              }
              onClick={() => setBottomTab("debug")}
              type="button"
            >
              Debug
            </button>
            <button
              className={
                "bottom-tab" +
                (bottomTab === "terminal" ? " bottom-tab-active" : "")
              }
              onClick={() => setBottomTab("terminal")}
              type="button"
            >
              Terminal
            </button>

            <button
              className={
                "bottom-tab" + (bottomTab === "failures" ? " bottom-tab-active" : "")
              }
              onClick={() => setBottomTab("failures")}
              type="button"
            >
              Failures
              {failures.length > 0 ? ` (${failures.length})` : ""}
            </button>

            {bottomTab === "debug" && events.length > 0 && (
              <span
                className="pane-header-count"
                aria-label={`${events.length} event${events.length === 1 ? "" : "s"}`}
              >
                {events.length}
                {events.length >= EVENTS_CAP && "+"}
              </span>
            )}

            <div style={{ flex: 1 }} />
            {debugOpen && bottomTab === "debug" && (
              <button
                onClick={clearEvents}
                style={{ fontSize: 10, padding: "2px 6px" }}
              >
                clear
              </button>
            )}
            {debugOpen && bottomTab === "failures" && projectDir && (
              <button
                onClick={() => {
                  void api.clearFailuresLog(projectDir).catch(() => {});
                  clearFailures();
                }}
                style={{ fontSize: 10, padding: "2px 6px" }}
              >
                clear
              </button>
            )}
          </div>

          {debugOpen && (
            <div className="pane-body bottom-body">
              {bottomTab === "debug" ? (
                <Execution events={events} />
              ) : bottomTab === "terminal" ? (
                <TerminalManager projectDir={projectDir} />
              ) : (
                <FailuresPanel failures={failures} />
              )}
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

function FailuresPanel({ failures }: { failures: FailureLogEntry[] }) {
  if (!failures || failures.length === 0) {
    return <div className="empty-state">No failures for this project yet.</div>;
  }
  return (
    <div className="task-failures" style={{ marginTop: 0 }}>
      <div className="task-failures-header">Recent failures ({failures.length})</div>
      <ul className="task-failures-list">
        {failures.map((f) => (
          <li key={`${f.task_id}-${f.at}`} className="task-failure-row">
            <span className="task-failure-time">
              {new Date(f.at * 1000).toLocaleTimeString()}
            </span>
            <span className="task-failure-id" title={f.task_id}>
              {f.task_id.slice(0, 8)}
            </span>
            <span
              className="task-failure-msg"
              title={f.error.length > 200 ? f.error : undefined}
            >
              {f.error.length > 120 ? f.error.slice(0, 119) + "…" : f.error}
            </span>
          </li>
        ))}
      </ul>
    </div>
  );
}
