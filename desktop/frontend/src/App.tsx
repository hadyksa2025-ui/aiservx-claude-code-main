import { useCallback, useEffect, useMemo, useState } from "react";
import { open as openDialog } from "@tauri-apps/plugin-dialog";
import { api, onEvent } from "./api";
import type {
  AgentRole,
  ChatMessage,
  ExecutionEvent,
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

/**
 * Maximum number of entries the in-memory execution-event ring buffer
 * keeps. Once reached, the oldest entries are dropped so long
 * autonomous runs can't OOM the renderer. Audit §7.7 / addendum §2.
 */
const EVENTS_CAP = 500;

export default function App() {
  const [projectDir, setProjectDir] = useState<string | null>(null);
  const [plannerOk, setPlannerOk] = useState<boolean | null>(null);
  const [executorOk, setExecutorOk] = useState<boolean | null>(null);
  const [messages, setMessages] = useState<ChatMessage[]>([]);
  const [events, setEvents] = useState<ExecutionEvent[]>([]);
  const [fsTick, setFsTick] = useState(0);
  const [settingsOpen, setSettingsOpen] = useState(false);
  // The execution pane is a developer-facing debug view. Collapsed by
  // default so a fresh install doesn't greet the user with a raw
  // event stream; pops back open automatically on the first error so
  // failures are never silently hidden.
  const [debugOpen, setDebugOpen] = useState(false);
  const [explorerOpen, setExplorerOpen] = useState(true);

  /**
   * Append an entry to the execution-event log and trim the tail so the
   * buffer can never balloon past {@link EVENTS_CAP} entries. Without
   * this cap a long autonomous run emits a step every few seconds plus
   * every tool call/result, and the `Execution` list grew unbounded —
   * audit §7.7 / addendum §2 flagged this as a production risk
   * (memory churn + scroll jank). Keeping only the tail is safe
   * because the UI already shows newest-first with a "clear" button.
   */
  const pushEvent = useCallback((e: ExecutionEvent) => {
    setEvents((prev) => {
      const next = prev.length >= EVENTS_CAP ? prev.slice(-(EVENTS_CAP - 1)) : prev;
      return [...next, e];
    });
  }, []);

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
  }, []);

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
      onEvent<{ message: string; role?: AgentRole }>("ai:error", (p) => {
        pushEvent({
          kind: "error",
          text: p.message,
          role: p.role,
          at: Date.now(),
        });
        // Surface the debug panel the moment anything goes wrong — a
        // silent collapsed panel on a failed run is exactly the kind
        // of UX the audit flagged.
        setDebugOpen(true);
      }),
    );
    unlistens.push(
      onEvent<FsChange>("fs:changed", () => setFsTick((t) => t + 1)),
    );
    return () => {
      for (const p of unlistens) {
        void p.then((fn) => fn());
      }
    };
    // `pushEvent` is stable — it's a useCallback with no deps.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // Watcher lifecycle tied to the opened project.
  useEffect(() => {
    if (!projectDir) return;
    let cancelled = false;
    void (async () => {
      try {
        await api.watchDir(projectDir);
      } catch (e) {
        pushEvent({
          kind: "error",
          text: `Failed to start watcher: ${String(e)}`,
          at: Date.now(),
        });
      }
    })();
    return () => {
      if (cancelled) return;
      cancelled = true;
      void api.unwatchDir(projectDir).catch(() => {});
    };
  }, [projectDir]);

  const openProject = useCallback(async () => {
    const selected = await openDialog({
      multiple: false,
      directory: true,
      title: "Open project",
    });
    if (typeof selected === "string" && selected.length > 0) {
      setProjectDir(selected);
      setMessages([]);
      setEvents([
        {
          kind: "info",
          text: `Opened project: ${selected}`,
          at: Date.now(),
        },
      ]);
    }
  }, []);

  const statusPlanner = useMemo(() => {
    if (plannerOk === null) return "checking…";
    return plannerOk ? "planner ready" : "planner off";
  }, [plannerOk]);

  const statusExecutor = useMemo(() => {
    if (executorOk === null) return "checking…";
    return executorOk ? "ollama online" : "ollama offline";
  }, [executorOk]);

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
              onClick={() => setExplorerOpen((v) => !v)}
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
            <TaskPanel
              projectDir={projectDir}
              disabled={!projectDir || executorOk === false}
            />
          </div>
        </section>

        <section className="pane" aria-label="Chat">
          <div className="pane-header">Chat</div>
          <div className="pane-body chat">
            <Chat
              projectDir={projectDir}
              messages={messages}
              setMessages={setMessages}
              disabled={!projectDir || executorOk === false}
            />
          </div>
        </section>

        <section
          className={`pane pane-debug${debugOpen ? "" : " pane-collapsed"}`}
          aria-label="Debug"
        >
          <div className="pane-header">
            <button
              className="pane-toggle"
              onClick={() => setDebugOpen((v) => !v)}
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
                onClick={() => setEvents([])}
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
