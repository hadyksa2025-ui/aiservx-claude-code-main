import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { api, onEvent } from "../api";
import type {
  Task,
  TaskAddedEvent,
  TaskGoalDoneEvent,
  TaskGoalStarted,
  TaskStatus,
  TaskTree,
  TaskUpdateEvent,
} from "../types";

type Props = {
  projectDir: string | null;
  disabled?: boolean;
};

type RunState = "idle" | "running" | "done" | "failed" | "cancelled";

export function TaskPanel({ projectDir, disabled }: Props) {
  const [goal, setGoal] = useState("");
  const [tree, setTree] = useState<TaskTree | null>(null);
  const [runState, setRunState] = useState<RunState>("idle");
  const [summary, setSummary] = useState<string | null>(null);
  const runningRef = useRef(false);

  // Load any previously-persisted active tree when the project opens.
  useEffect(() => {
    if (!projectDir) {
      setTree(null);
      setRunState("idle");
      setSummary(null);
      return;
    }
    void api
      .loadTaskTree(projectDir)
      .then((loaded) => {
        if (loaded && typeof loaded === "object" && "tasks" in loaded) {
          setTree(loaded);
          setRunState(loaded.status === "running" ? "idle" : (loaded.status as RunState));
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
      }),
    );
    unlistens.push(
      onEvent<TaskAddedEvent>("task:added", (p) => {
        setTree((prev) => {
          if (!prev || prev.id !== p.goal_id) return prev;
          if (prev.tasks.find((t) => t.id === p.task.id)) return prev;
          return { ...prev, tasks: [...prev.tasks, p.task] };
        });
      }),
    );
    unlistens.push(
      onEvent<TaskUpdateEvent>("task:update", (p) => {
        setTree((prev) => {
          if (!prev || prev.id !== p.goal_id) return prev;
          return {
            ...prev,
            tasks: prev.tasks.map((t) =>
              t.id === p.id
                ? {
                    ...t,
                    status: p.status ?? t.status,
                    retries: p.retries_bumped ? t.retries + 1 : p.retries ?? t.retries,
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
    try {
      await api.startGoal(projectDir, goal.trim());
    } catch (e) {
      setRunState("failed");
      setSummary(`Goal failed: ${String(e)}`);
      runningRef.current = false;
    }
  }, [projectDir, goal]);

  const cancelGoal = useCallback(async () => {
    try {
      await api.cancelGoal();
    } catch {
      // ignore
    }
  }, []);

  const progress = useMemo(() => {
    if (!tree || tree.tasks.length === 0) return { done: 0, total: 0, pct: 0 };
    const done = tree.tasks.filter(
      (t) => t.status === "done" || t.status === "failed" || t.status === "skipped",
    ).length;
    return {
      done,
      total: tree.tasks.length,
      pct: Math.round((done / tree.tasks.length) * 100),
    };
  }, [tree]);

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

      {tree && (
        <div className="task-tree">
          <div className="task-tree-header">
            <span className={`task-status-chip task-status-${runState}`}>
              {runState}
            </span>
            <span className="task-progress">
              {progress.done}/{progress.total} · {progress.pct}%
            </span>
            <div className="task-progress-bar">
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
              <TaskRow key={t.id} index={i + 1} task={t} />
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

function TaskRow({ index, task }: { index: number; task: Task }) {
  return (
    <li className={`task-row task-row-${task.status}`}>
      <span className={`task-dot task-dot-${task.status}`} />
      <span className="task-index">{index}.</span>
      <div className="task-body">
        <div className="task-desc">{task.description}</div>
        {task.retries > 0 && (
          <div className="task-retries">retries: {task.retries}</div>
        )}
        {task.result && (
          <div className="task-result">{task.result}</div>
        )}
      </div>
      <span className={`task-badge task-badge-${task.status}`}>
        {statusLabel(task.status)}
      </span>
    </li>
  );
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
