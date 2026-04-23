import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { api, onEvent } from "../api";
import { useAppStore } from "../store";
import type {
  Task,
  TaskAddedEvent,
  TaskCircuitTrippedEvent,
  TaskGoalDoneEvent,
  TaskGoalStarted,
  TaskStatus,
  TaskTrace,
  TaskTraceEvent,
  TaskTree,
  TaskUpdateEvent,
  TraceEntry,
} from "../types";

type Props = {
  projectDir: string | null;
  disabled?: boolean;
};

type RunState = "idle" | "running" | "done" | "failed" | "cancelled" | "timeout";

export function TaskPanel({ projectDir, disabled }: Props) {
  const [goal, setGoal] = useState("");
  const [tree, setTree] = useState<TaskTree | null>(null);
  const [runState, setRunState] = useState<RunState>("idle");
  const [summary, setSummary] = useState<string | null>(null);
  const [circuitTripped, setCircuitTripped] =
    useState<TaskCircuitTrippedEvent | null>(null);
  const runningRef = useRef(false);

  // Scenario-A §9.2 F-2: the backend emits `goal:planning` /
  // `goal:planning_done` while the project scan and planner stream are
  // running. App.tsx stores the current phase; we render a
  // lightweight chip here so the pane isn't silent for the 2+ minute
  // pre-execution window on small local models.
  const goalPlanning = useAppStore((s) => s.goalPlanning);
  // OC-Titan §VI.2/§VI.3 — pipeline state-machine slice.
  const pipelinePhase = useAppStore((s) => s.pipelinePhase);
  const pipelineAttempt = useAppStore((s) => s.pipelineAttempt);
  const pipelineLastLabel = useAppStore((s) => s.pipelineLastLabel);
  const resetPipeline = useAppStore((s) => s.resetPipeline);

  // Load any previously-persisted active tree and failures log when the
  // project opens.
  useEffect(() => {
    if (!projectDir) {
      setTree(null);
      setRunState("idle");
      setSummary(null);
      setCircuitTripped(null);
      return;
    }
    void api
      .loadTaskTree(projectDir)
      .then((loaded) => {
        if (loaded && typeof loaded === "object" && "tasks" in loaded) {
          setTree(loaded);
          // If we find a stale "running" tree on load, the engine isn't
          // actually running any more — reconcile to idle so the UI doesn't
          // lie.
          setRunState(
            loaded.status === "running" ? "idle" : (loaded.status as RunState),
          );
        }
      })
      .catch(() => {});
  }, [projectDir]);

  // Subscribe to task lifecycle events.
  useEffect(() => {
    const unlistens: Array<Promise<() => void>> = [];
    unlistens.push(
      onEvent<TaskGoalStarted>("task:goal_started", (p) => {
        setTree({
          id: p.id,
          goal: p.goal,
          tasks: [],
          created_at: p.created_at,
          updated_at: p.created_at,
          status: "running",
        });
        setRunState("running");
        setSummary(null);
        setCircuitTripped(null);
      }),
    );
    unlistens.push(
      onEvent<TaskAddedEvent>("task:added", (p) => {
        setTree((prev) => {
          if (!prev || prev.id !== p.goal_id) return prev;
          // Dedupe by id — the backend emits at most once per task, but
          // React StrictMode (dev) invokes effects twice and we also want
          // to be defensive against any future replays.
          if (prev.tasks.some((t) => t.id === p.task.id)) return prev;
          return { ...prev, tasks: [...prev.tasks, p.task] };
        });
      }),
    );
    unlistens.push(
      onEvent<TaskUpdateEvent>("task:update", (p) => {
        setTree((prev) => {
          if (!prev || prev.id !== p.goal_id) return prev;
          // Upsert: if the update arrives before the matching `task:added`
          // (rare but possible under burst conditions), synthesize a
          // minimal row so the UI doesn't drop the signal. Backend ships
          // the real description on every `task:update` so the row gets
          // a meaningful label instead of a placeholder.
          const existing = prev.tasks.find((t) => t.id === p.id);
          if (!existing) {
            const now = Math.floor(Date.now() / 1000);
            const synthesized: Task = {
              id: p.id,
              description: p.description ?? "(task)",
              status: (p.status ?? "pending") as TaskStatus,
              retries: p.retries ?? 0,
              deps: [],
              result: p.result ?? null,
              created_at: now,
              updated_at: p.updated_at ?? now,
            };
            return { ...prev, tasks: [...prev.tasks, synthesized] };
          }
          return {
            ...prev,
            tasks: prev.tasks.map((t) =>
              t.id === p.id
                ? {
                    ...t,
                    // Let the backend correct a placeholder description
                    // the moment a real one becomes available, but never
                    // overwrite a genuine description with a placeholder.
                    description: p.description ?? t.description,
                    status: p.status ?? t.status,
                    // Prefer the explicit retries count from the backend;
                    // only fall back to the `retries_bumped` signal when
                    // the explicit count is absent.
                    retries:
                      typeof p.retries === "number"
                        ? p.retries
                        : p.retries_bumped
                          ? t.retries + 1
                          : t.retries,
                    result: p.result ?? t.result,
                    updated_at: p.updated_at ?? t.updated_at,
                  }
                : t,
            ),
          };
        });
      }),
    );
    unlistens.push(
      onEvent<TaskCircuitTrippedEvent>("task:circuit_tripped", (p) => {
        setCircuitTripped(p);
      }),
    );
    unlistens.push(
      onEvent<TaskTraceEvent>("task:trace", (p) => {
        // Apply the fresh trace blob to the matching task. The backend
        // already enforces per-trace size caps, so we just replace.
        setTree((prev) => {
          if (!prev || prev.id !== p.goal_id) return prev;
          const i = prev.tasks.findIndex((t) => t.id === p.id);
          if (i < 0) return prev;
          const next = prev.tasks.slice();
          next[i] = {
            ...next[i],
            trace: p.trace,
            updated_at: p.updated_at ?? next[i].updated_at,
          };
          return { ...prev, tasks: next };
        });
      }),
    );
    unlistens.push(
      onEvent<TaskGoalDoneEvent>("task:goal_done", (p) => {
        setTree((prev) => (prev ? { ...prev, status: p.status } : prev));
        setRunState(p.status as RunState);
        setSummary(
          `${p.completed} completed, ${p.failed} failed — ${p.status}`,
        );
        runningRef.current = false;
      }),
    );
    return () => {
      for (const pr of unlistens) void pr.then((fn) => fn());
    };
  }, []);

  const startGoal = useCallback(async () => {
    if (!projectDir || !goal.trim() || runningRef.current) return;
    runningRef.current = true;
    setRunState("running");
    setSummary(null);
    setCircuitTripped(null);
    // OC-Titan §VI.2/§VI.3 — reset pipeline slice at the start of a
    // new goal turn, so the state machine starts cleanly at `idle`
    // and stale events from a prior run are not rendered under the
    // new goal.
    resetPipeline();
    try {
      await api.startGoal(projectDir, goal.trim());
    } catch (e) {
      setRunState("failed");
      setSummary(`Goal failed: ${String(e)}`);
      runningRef.current = false;
    }
  }, [projectDir, goal, resetPipeline]);

  const cancelGoal = useCallback(async () => {
    try {
      await api.cancelGoal();
    } catch {
      // ignore
    }
  }, []);

  const progress = useMemo(() => {
    if (!tree || tree.tasks.length === 0)
      return { done: 0, total: 0, pct: 0, succeeded: 0, failed: 0, running: 0 };
    const succeeded = tree.tasks.filter((t) => t.status === "done").length;
    const failed = tree.tasks.filter((t) => t.status === "failed").length;
    const skipped = tree.tasks.filter((t) => t.status === "skipped").length;
    const running = tree.tasks.filter((t) => t.status === "running").length;
    const done = succeeded + failed + skipped;
    return {
      done,
      total: tree.tasks.length,
      pct: Math.round((done / tree.tasks.length) * 100),
      succeeded,
      failed,
      running,
    };
  }, [tree]);

  // 1s clock for the live elapsed-time display on any running task /
  // the overall goal duration. Only ticks while the run is active so
  // idle projects don't re-render the tree every second.
  const [nowSec, setNowSec] = useState(() => Math.floor(Date.now() / 1000));
  useEffect(() => {
    if (runState !== "running") return;
    const id = setInterval(
      () => setNowSec(Math.floor(Date.now() / 1000)),
      1000,
    );
    return () => clearInterval(id);
  }, [runState]);

  const goalDurationSec = useMemo(() => {
    if (!tree) return null;
    const ref =
      runState === "running"
        ? nowSec
        : Math.max(tree.updated_at, tree.created_at);
    return Math.max(0, ref - tree.created_at);
  }, [tree, nowSec, runState]);

  return (
    <div className="task-panel">
      <div className="task-goal-row">
        <textarea
          className="task-goal-input"
          placeholder={
            projectDir
              ? "Describe a high-level goal, e.g. 'Refactor this project to improve structure'"
              : "Open a project to set a goal."
          }
          value={goal}
          onChange={(e) => setGoal(e.target.value)}
          disabled={disabled || runState === "running"}
          rows={2}
        />
        <div className="task-goal-actions">
          <button
            onClick={() => void startGoal()}
            disabled={
              disabled || !projectDir || !goal.trim() || runState === "running"
            }
          >
            {runState === "running" ? "Running…" : "Start goal"}
          </button>
          <button
            onClick={() => void cancelGoal()}
            disabled={runState !== "running"}
          >
            Cancel
          </button>
        </div>
      </div>

      {circuitTripped && (
        <div className="task-circuit-banner">
          Circuit breaker tripped after {circuitTripped.consecutive_failures}{" "}
          consecutive failures (threshold {circuitTripped.threshold}). Goal
          halted.
        </div>
      )}

      {goalPlanning && (
        <div
          className={`task-planning-chip task-planning-chip-${goalPlanning}`}
          role="status"
          aria-live="polite"
        >
          <span className="task-planning-spinner" aria-hidden>
            ⋯
          </span>
          {goalPlanning === "scanning"
            ? "Scanning project…"
            : "Planner drafting task list…"}
        </div>
      )}

      {pipelinePhase !== "idle" && (
        <div
          className={`task-pipeline-chip task-pipeline-chip-${pipelinePhase}`}
          role="status"
          aria-live="polite"
          title={
            pipelineLastLabel
              ? `last event: ${pipelineLastLabel} · attempt ${pipelineAttempt}`
              : `pipeline phase: ${pipelinePhase}`
          }
        >
          <span className="task-pipeline-label">
            pipeline: {pipelinePhase.replace("_", " ")}
          </span>
          {pipelineAttempt > 0 && (
            <span className="task-pipeline-attempt">
              · attempt {pipelineAttempt}
            </span>
          )}
          {pipelineLastLabel && (
            <span className="task-pipeline-last">· {pipelineLastLabel}</span>
          )}
        </div>
      )}

      {tree && (
        <div className="task-tree">
          <div className="task-tree-header">
            <span
              className={`task-status-chip task-status-${runState}`}
              aria-label={`goal ${runState}`}
            >
              <span className={`task-icon task-icon-goal-${runState}`} aria-hidden>
                {runStateIcon(runState)}
              </span>
              {runState}
            </span>
            <span className="task-progress">
              {progress.done}/{progress.total} · {progress.pct}%
            </span>
            {progress.running > 0 && (
              <span className="task-progress-running" title="tasks currently running">
                {progress.running} running
              </span>
            )}
            {progress.failed > 0 && (
              <span className="task-progress-failed" title="tasks that failed">
                {progress.failed} failed
              </span>
            )}
            {goalDurationSec != null && (
              <span
                className="task-goal-duration"
                aria-label={`goal ${
                  runState === "running" ? "elapsed" : "duration"
                }`}
              >
                {formatDurationSec(goalDurationSec)}
              </span>
            )}
            <div
              className={`task-progress-bar task-progress-bar-${runState}`}
              role="progressbar"
              aria-valuenow={progress.pct}
              aria-valuemin={0}
              aria-valuemax={100}
              aria-label="goal progress"
            >
              <div
                className="task-progress-fill"
                style={{ width: `${progress.pct}%` }}
              />
            </div>
          </div>
          <div className="task-tree-goal">{tree.goal}</div>
          {summary && <div className="task-summary">{summary}</div>}
          <ol className="task-list">
            {tree.tasks.map((t, i) => (
              <TaskRow
                key={t.id}
                index={i + 1}
                task={t}
                trace={t.trace}
                nowSec={nowSec}
              />
            ))}
            {tree.tasks.length === 0 && (
              <li className="task-empty">
                Planning… (waiting for task list from planner)
              </li>
            )}
          </ol>
        </div>
      )}

      {!tree && (
        <div className="empty-state">
          The autonomous task engine will decompose your goal into tasks and
          execute them in order. You will see each task's status here.
        </div>
      )}
    </div>
  );
}

function TaskRow({
  index,
  task,
  trace,
  nowSec,
}: {
  index: number;
  task: Task;
  trace?: TaskTrace;
  /** Current clock (epoch seconds) — used to compute elapsed time for
   *  running tasks. Passed in from the parent so every row ticks off
   *  the same timer. */
  nowSec: number;
}) {
  const [open, setOpen] = useState(false);
  const hasTrace = !!trace && trace.entries.length > 0;

  // Pick the freshest "thing happening right now" to surface on the
  // row header so the user doesn't have to open the trace just to see
  // that a long-running tool call is still alive. For completed tasks
  // this shows the reason the task failed (if any) so the 200-entry
  // trace isn't the first place the user has to look.
  const liveAction = useMemo(
    () => pickLiveAction(task, trace),
    [task, trace],
  );

  const durationSec =
    task.status === "running"
      ? Math.max(0, nowSec - task.created_at)
      : task.status === "pending"
        ? null
        : Math.max(0, (task.updated_at ?? task.created_at) - task.created_at);

  return (
    <li className={`task-row task-row-${task.status}`}>
      <span
        className={`task-icon task-icon-${task.status}`}
        aria-hidden
        title={statusLabel(task.status)}
      >
        {statusIcon(task.status)}
      </span>
      <span className="task-index">{index}.</span>
      <div className="task-body">
        <div className="task-desc">{task.description}</div>
        {liveAction && (
          <div className={`task-live-action task-live-${liveAction.tone}`}>
            <span className="task-live-arrow" aria-hidden>
              {liveAction.tone === "error" ? "✗" : "→"}
            </span>
            <span className="task-live-text">{liveAction.text}</span>
          </div>
        )}
        {task.retries > 0 && (
          <div className="task-retries">
            {task.retries} retr{task.retries === 1 ? "y" : "ies"}
          </div>
        )}
        {task.result && task.status !== "running" && (
          <div
            className="task-result"
            title={task.result.length > 300 ? task.result : undefined}
          >
            {condenseResult(task.result)}
          </div>
        )}
        {hasTrace && (
          <button
            className="task-trace-toggle"
            onClick={() => setOpen((v) => !v)}
            aria-expanded={open}
          >
            {open ? "▾" : "▸"} Trace ({trace!.entries.length}
            {trace!.truncated ? "+" : ""} entries)
          </button>
        )}
        {open && hasTrace && <TraceView trace={trace!} />}
      </div>
      <div className="task-row-meta">
        {durationSec != null && (
          <span className="task-duration" aria-label="elapsed">
            {formatDurationSec(durationSec)}
          </span>
        )}
        <span className={`task-badge task-badge-${task.status}`}>
          {statusLabel(task.status)}
        </span>
      </div>
    </li>
  );
}

function runStateIcon(s: RunState): string {
  switch (s) {
    case "running":
      return "⋯";
    case "done":
      return "✓";
    case "failed":
      return "✗";
    case "cancelled":
      return "⊘";
    case "timeout":
      return "⏱";
    case "idle":
    default:
      return "○";
  }
}

function statusIcon(s: TaskStatus): string {
  switch (s) {
    case "pending":
      return "○";
    case "running":
      return "⋯";
    case "done":
      return "✓";
    case "failed":
      return "✗";
    case "skipped":
      return "⊘";
  }
}

/** Format elapsed / duration in seconds as `Ns` / `M:SS` / `Hh Mm`. */
function formatDurationSec(sec: number): string {
  if (sec < 1) return "<1s";
  if (sec < 60) return `${Math.floor(sec)}s`;
  if (sec < 3600) {
    const m = Math.floor(sec / 60);
    const s = Math.floor(sec % 60);
    return `${m}:${s.toString().padStart(2, "0")}`;
  }
  const h = Math.floor(sec / 3600);
  const m = Math.floor((sec % 3600) / 60);
  return `${h}h ${m.toString().padStart(2, "0")}m`;
}

/** Pick a single short line describing what's happening on this task
 *  right now. For running tasks we surface the newest tool call; for
 *  failed tasks we surface the first error entry; for done tasks we
 *  return null so the UI stays quiet. */
function pickLiveAction(
  task: Task,
  trace: TaskTrace | undefined,
): { text: string; tone: "info" | "error" } | null {
  if (!trace || trace.entries.length === 0) return null;
  if (task.status === "running") {
    for (let i = trace.entries.length - 1; i >= 0; i--) {
      const e = trace.entries[i];
      if (e.kind === "tool_call") {
        return {
          text: `${e.name} ${condenseArgs(e.args)}`,
          tone: "info",
        };
      }
      if (e.kind === "error") {
        return { text: e.message, tone: "error" };
      }
    }
    return null;
  }
  if (task.status === "failed") {
    for (const e of trace.entries) {
      if (e.kind === "error") {
        return { text: e.message, tone: "error" };
      }
    }
    for (const e of trace.entries) {
      if (e.kind === "tool_result" && !e.ok) {
        return {
          text: condenseResult(e.output),
          tone: "error",
        };
      }
    }
  }
  return null;
}

/** Collapse a multi-line / JSON-blob result down to a single-line summary. */
function condenseResult(s: string): string {
  const one = s.replace(/\s+/g, " ").trim();
  const MAX = 160;
  return one.length > MAX ? one.slice(0, MAX - 1) + "…" : one;
}

/** Trim a tool-call args blob to a short preview so the row doesn't
 *  inherit a 400-character JSON dump on every row. */
function condenseArgs(args: string): string {
  const one = args.replace(/\s+/g, " ").trim();
  const MAX = 80;
  return one.length > MAX ? one.slice(0, MAX - 1) + "…" : one;
}

function TraceView({ trace }: { trace: TaskTrace }) {
  return (
    <ol className="task-trace-list">
      {trace.entries.map((e, i) => (
        <li key={i} className={`task-trace-entry task-trace-${e.kind}`}>
          <TraceEntryRow entry={e} />
        </li>
      ))}
      {trace.truncated && (
        <li className="task-trace-truncated">
          older entries were truncated to stay within the per-task cap
        </li>
      )}
    </ol>
  );
}

function TraceEntryRow({ entry }: { entry: TraceEntry }) {
  switch (entry.kind) {
    case "user":
      return (
        <>
          <span className="task-trace-label">user</span>
          <span className="task-trace-text">{entry.text}</span>
        </>
      );
    case "plan":
      return (
        <>
          <span className="task-trace-label">plan</span>
          <pre className="task-trace-block">{entry.text}</pre>
        </>
      );
    case "assistant":
      return (
        <>
          <span className="task-trace-label">{entry.role}</span>
          <span className="task-trace-text">{entry.text}</span>
        </>
      );
    case "tool_call":
      return (
        <>
          <span className="task-trace-label">→ {entry.name}</span>
          <pre className="task-trace-block">{entry.args}</pre>
        </>
      );
    case "tool_result":
      return (
        <>
          <span
            className={`task-trace-label task-trace-${
              entry.ok ? "ok" : "err"
            }`}
          >
            {entry.ok ? "← ok" : "← err"}
          </span>
          <pre className="task-trace-block">
            {entry.output}
            {entry.diff ? `\n${entry.diff}` : ""}
          </pre>
        </>
      );
    case "review":
      return (
        <>
          <span className="task-trace-label">review: {entry.verdict}</span>
          <span className="task-trace-text">{entry.text}</span>
        </>
      );
    case "retry":
      return (
        <>
          <span className="task-trace-label">retry #{entry.attempt}</span>
          <span className="task-trace-text">{entry.reason}</span>
        </>
      );
    case "error":
      return (
        <>
          <span className="task-trace-label task-trace-err">
            error ({entry.role})
          </span>
          <span className="task-trace-text">{entry.message}</span>
        </>
      );
  }
}

function statusLabel(s: TaskStatus): string {
  switch (s) {
    case "pending":
      return "pending";
    case "running":
      return "running";
    case "done":
      return "done";
    case "failed":
      return "failed";
    case "skipped":
      return "skipped";
  }
}
