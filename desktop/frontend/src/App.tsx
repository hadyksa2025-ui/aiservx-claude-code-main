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

export default function App() {
  const [projectDir, setProjectDir] = useState<string | null>(null);
  const [plannerOk, setPlannerOk] = useState<boolean | null>(null);
  const [executorOk, setExecutorOk] = useState<boolean | null>(null);
  const [messages, setMessages] = useState<ChatMessage[]>([]);
  const [events, setEvents] = useState<ExecutionEvent[]>([]);
  const [fsTick, setFsTick] = useState(0);
  const [settingsOpen, setSettingsOpen] = useState(false);

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
        setEvents((prev) => [
          ...prev,
          { kind: "tool_call", call: p, at: Date.now() },
        ]),
      ),
    );
    unlistens.push(
      onEvent<ToolResult>("ai:tool_result", (p) =>
        setEvents((prev) => [
          ...prev,
          { kind: "tool_result", result: p, at: Date.now() },
        ]),
      ),
    );
    unlistens.push(
      onEvent<StepEvent>("ai:step", (p) =>
        setEvents((prev) => [
          ...prev,
          { kind: "step", step: p, at: Date.now() },
        ]),
      ),
    );
    unlistens.push(
      onEvent<{ message: string; role?: AgentRole }>("ai:error", (p) =>
        setEvents((prev) => [
          ...prev,
          { kind: "error", text: p.message, role: p.role, at: Date.now() },
        ]),
      ),
    );
    unlistens.push(
      onEvent<FsChange>("fs:changed", () => setFsTick((t) => t + 1)),
    );
    return () => {
      for (const p of unlistens) {
        void p.then((fn) => fn());
      }
    };
  }, []);

  // Watcher lifecycle tied to the opened project.
  useEffect(() => {
    if (!projectDir) return;
    let cancelled = false;
    void (async () => {
      try {
        await api.watchDir(projectDir);
      } catch (e) {
        setEvents((prev) => [
          ...prev,
          {
            kind: "error",
            text: `Failed to start watcher: ${String(e)}`,
            at: Date.now(),
          },
        ]);
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
      <div className="topbar">
        <h1>Open Claude Code</h1>
        <button onClick={openProject}>Open project…</button>
        <span className="project-path">{projectDir ?? "no project"}</span>
        <div className="spacer" />
        <span className="status-badge">
          <span
            className={
              "status-dot " +
              (plannerOk === null ? "" : plannerOk ? "ok" : "bad")
            }
          />
          {statusPlanner}
        </span>
        <span className="status-badge">
          <span
            className={
              "status-dot " +
              (executorOk === null ? "" : executorOk ? "ok" : "bad")
            }
          />
          {statusExecutor}
        </span>
        <button onClick={() => setSettingsOpen(true)}>Settings</button>
      </div>

      <div className="panes panes-4">
        <div className="pane">
          <div className="pane-header">Explorer</div>
          <div className="pane-body">
            {projectDir ? (
              <Explorer key={projectDir + ":" + fsTick} projectDir={projectDir} />
            ) : (
              <div className="empty-state">
                Open a project folder to see its files.
              </div>
            )}
          </div>
        </div>

        <div className="pane">
          <div className="pane-header">Goal &amp; Tasks</div>
          <div className="pane-body">
            <TaskPanel
              projectDir={projectDir}
              disabled={!projectDir || executorOk === false}
            />
          </div>
        </div>

        <div className="pane">
          <div className="pane-header">Chat</div>
          <div className="pane-body chat">
            <Chat
              projectDir={projectDir}
              messages={messages}
              setMessages={setMessages}
              disabled={!projectDir || executorOk === false}
            />
          </div>
        </div>

        <div className="pane">
          <div className="pane-header">
            Execution
            <div style={{ flex: 1 }} />
            <button
              onClick={() => setEvents([])}
              style={{ fontSize: 10, padding: "2px 6px" }}
            >
              clear
            </button>
          </div>
          <div className="pane-body">
            <Execution events={events} />
          </div>
        </div>
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
