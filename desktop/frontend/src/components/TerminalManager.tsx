import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { Terminal } from "./Terminal";
import { api } from "../api";

type TerminalTab = {
  id: string;
  title: string;
  /**
   * Pinned tabs are created by the app itself (currently: the "Agent"
   * tab that mirrors every AI tool call). They cannot be closed and
   * are not recycled on project change. See PROJECT_MEMORY.md §12
   * Terminal Authority.
   */
  pinned?: "agent";
};

/** Stable id the backend uses for every AI-tool `terminal:output` event. */
const AGENT_TERMINAL_ID = "agent-main";

function newTerminalId(): string {
  if (typeof crypto !== "undefined" && "randomUUID" in crypto) {
    return crypto.randomUUID();
  }
  return `term_${Date.now()}_${Math.random().toString(16).slice(2)}`;
}

/**
 * Multi-terminal manager.
 *
 * Lifecycle note: the set of currently-running child processes is tracked
 * in a `useRef` — *not* React state — and mutated imperatively from
 * `handleRunningChange`. It is deliberately **not** a `useEffect`
 * dependency. PROJECT_MEMORY.md §10.1 A-1 explains why: an earlier
 * version kept running-ids in state and listed it in the project-change
 * effect's dep array, which meant every time a child process flipped
 * `running → true` the parent effect re-fired, killed the freshly-
 * spawned child, and replaced the tabs mid-use. The ref carries the
 * same information without causing renders, so the effect's *only*
 * legitimate dependency is `projectDir`.
 */
export function TerminalManager({ projectDir }: { projectDir: string | null }) {
  const [tabs, setTabs] = useState<TerminalTab[]>(() => [
    { id: AGENT_TERMINAL_ID, title: "Agent", pinned: "agent" },
    { id: newTerminalId(), title: "Terminal 1" },
  ]);
  const [activeId, setActiveId] = useState(() => tabs[0]!.id);
  const runningRef = useRef<Set<string>>(new Set());

  useEffect(() => {
    // Snapshot so we don't race with handleRunningChange mutations while
    // the kill RPCs are still in flight. Pinned tabs (Agent) are driven
    // by the AI tool loop, not by a direct spawn this component owns, so
    // they aren't in `runningRef` and aren't killed here.
    const toKill = Array.from(runningRef.current);
    runningRef.current.clear();

    void (async () => {
      for (const terminalId of toKill) {
        try {
          await api.terminalKill(terminalId);
        } catch {
          // Ignore — process may have already ended.
        }
      }
    })();

    // New project == new terminal sessions (avoid cross-project mixing).
    // Keep the pinned Agent tab — it represents the AI execution surface,
    // not a user session, and is always-on. Only user-driven tabs are
    // recycled here.
    const userTabId = newTerminalId();
    setTabs([
      { id: AGENT_TERMINAL_ID, title: "Agent", pinned: "agent" },
      { id: userTabId, title: "Terminal 1" },
    ]);
    setActiveId(userTabId);
  }, [projectDir]);

  useEffect(() => {
    // Keep activeId valid after resets.
    if (!tabs.some((t) => t.id === activeId)) {
      setActiveId(tabs[0]!.id);
    }
  }, [tabs, activeId]);

  const active = useMemo(
    () => tabs.find((t) => t.id === activeId) ?? tabs[0]!,
    [tabs, activeId],
  );

  const addTab = useCallback(() => {
    setTabs((prev) => {
      // Numbering is over *user* tabs only — the pinned Agent tab is an
      // always-on app surface, not a user session, and must not inflate
      // the count (otherwise the first user-added tab would be
      // "Terminal 3" instead of "Terminal 2").
      const nextIndex = prev.filter((t) => !t.pinned).length + 1;
      const next = { id: newTerminalId(), title: `Terminal ${nextIndex}` };
      return [...prev, next];
    });
  }, []);

  const handleRunningChange = useCallback(
    (terminalId: string, running: boolean) => {
      if (running) {
        runningRef.current.add(terminalId);
      } else {
        runningRef.current.delete(terminalId);
      }
    },
    [],
  );

  const closeTab = useCallback(async (id: string) => {
    // Pinned tabs (Agent) are lifecycle-owned by the app, not the user,
    // so the close button is hidden for them — but guard here too so
    // that a programmatic call can't accidentally drop the Agent tab.
    if (id === AGENT_TERMINAL_ID) return;
    if (runningRef.current.has(id)) {
      runningRef.current.delete(id);
      try {
        await api.terminalKill(id);
      } catch {
        // Ignore kill errors - process may have already ended
      }
    }
    setTabs((prev) => {
      // Always keep at least one user-driven tab in addition to the
      // pinned Agent tab, so the user has somewhere to type.
      const userTabs = prev.filter((t) => !t.pinned);
      if (userTabs.length <= 1) return prev;
      return prev.filter((t) => t.id !== id);
    });
  }, []);

  return (
    <div className="terminal-manager">
      <div className="terminal-manager-tabs" role="tablist" aria-label="Terminal tabs">
        {tabs.map((t) => (
          <div
            key={t.id}
            className={
              "terminal-manager-tab" +
              (t.id === activeId ? " terminal-manager-tab-active" : "") +
              (t.pinned === "agent" ? " terminal-manager-tab-agent" : "")
            }
            role="tab"
            aria-selected={t.id === activeId}
          >
            <button
              type="button"
              className="terminal-manager-tab-btn"
              onClick={() => setActiveId(t.id)}
              title={t.pinned === "agent" ? "Live AI tool execution" : t.title}
            >
              {t.pinned === "agent" ? (
                <>
                  <span className="terminal-manager-tab-agent-dot" aria-hidden="true">●</span>
                  {t.title}
                </>
              ) : (
                t.title
              )}
            </button>
            {!t.pinned && tabs.filter((x) => !x.pinned).length > 1 && (
              <button
                type="button"
                className="terminal-manager-tab-close"
                onClick={() => closeTab(t.id)}
                aria-label={`Close ${t.title}`}
                title="Close"
              >
                ×
              </button>
            )}
          </div>
        ))}
        <button
          type="button"
          className="terminal-manager-tab-add"
          onClick={addTab}
          aria-label="New terminal"
          title="New terminal"
        >
          +
        </button>
      </div>

      <div className="terminal-manager-body">
        {/*
         * Render ONE Terminal instance per tab simultaneously and hide
         * inactive ones with `display:none`. This preserves per-tab state
         * (lines buffer, running flag, scroll position) and — critically —
         * keeps each Terminal's `terminal:output` listener mounted at all
         * times. The Agent tab MUST stay mounted even while the user is
         * looking at another tab; otherwise every AI tool call emitted for
         * `terminal_id = "agent-main"` would be silently dropped, breaking
         * the Terminal Authority invariant in PROJECT_MEMORY.md §12.
         */}
        {tabs.map((t) => (
          <div
            key={t.id}
            className="terminal-manager-panel"
            style={{ display: t.id === active.id ? undefined : "none" }}
            role="tabpanel"
            aria-hidden={t.id !== active.id}
          >
            <Terminal
              projectDir={projectDir}
              terminalId={t.id}
              agentMode={t.pinned === "agent"}
              onRunningChange={(running) => handleRunningChange(t.id, running)}
            />
          </div>
        ))}
      </div>
    </div>
  );
}
