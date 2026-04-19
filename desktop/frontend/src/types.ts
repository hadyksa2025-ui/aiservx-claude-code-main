export type FsEntry = {
  name: string;
  path: string;
  is_dir: boolean;
  size: number | null;
};

export type FsChange = {
  path: string;
  kind: "created" | "modified" | "removed" | "renamed" | "other";
};

export type AgentRole = "planner" | "executor" | "reviewer";

export type ToolCall = {
  id: string;
  name: string;
  args: unknown;
  /** Which agent issued this tool call. */
  role?: AgentRole;
};

export type ToolResult = {
  id: string;
  ok: boolean;
  output: string;
  diff: string | null;
  role?: AgentRole;
};

export type ChatMessage = {
  id: string;
  role: "user" | "assistant" | "system" | "tool";
  content: string;
  tool_calls?: ToolCall[];
  tool_results?: ToolResult[];
  streaming?: boolean;
  /** Which agent authored the streaming partial, if any. */
  streaming_role?: AgentRole;
};

export type Settings = {
  openrouter_api_key: string;
  openrouter_model: string;
  ollama_base_url: string;
  ollama_model: string;
  reviewer_enabled: boolean;
  max_iterations: number;
  cmd_confirm_required: boolean;
  cmd_allow_list: string[];
  autonomous_mode: boolean;
  max_retries_per_task: number;
  max_total_tasks: number;
  task_timeout_secs: number;
  goal_timeout_secs: number;
  retry_backoff_base_ms: number;
  circuit_breaker_threshold: number;
  max_parallel_tasks: number;
  /**
   * When true, `write_file` (on destructive changes to existing files)
   * and `run_cmd` are routed through the confirm modal even when
   * `autonomous_mode` is on — the `cmd_allow_list` is bypassed for
   * irreversible operations. Chat-driven turns are unaffected.
   * Defaults to false so existing autonomous runs keep their
   * current behaviour.
   */
  autonomous_confirm_irreversible: boolean;
};

export type TaskStatus = "pending" | "running" | "done" | "failed" | "skipped";

export type Task = {
  id: string;
  description: string;
  status: TaskStatus;
  retries: number;
  deps: string[];
  result: string | null;
  created_at: number;
  updated_at: number;
  /** Execution transcript — populated by the autonomous controller. */
  trace?: TaskTrace;
};

/**
 * Bounded per-task execution transcript. Mirrors `trace::TaskTrace` in
 * the Rust backend. Only the kinds below are emitted; unknown kinds
 * MUST be ignored by the UI so future additions are non-breaking.
 */
export type TraceEntry =
  | { kind: "user"; text: string; at: number }
  | { kind: "plan"; text: string; at: number }
  | { kind: "assistant"; role: string; text: string; at: number }
  | {
      kind: "tool_call";
      id: string;
      role: string;
      name: string;
      args: string;
      at: number;
    }
  | {
      kind: "tool_result";
      id: string;
      role: string;
      ok: boolean;
      output: string;
      diff?: string | null;
      at: number;
    }
  | { kind: "review"; verdict: string; text: string; at: number }
  | { kind: "retry"; attempt: number; reason: string; at: number }
  | { kind: "error"; role: string; message: string; at: number };

export type TaskTrace = {
  entries: TraceEntry[];
  truncated: boolean;
};

export type TaskTraceEvent = {
  goal_id: string;
  id: string;
  trace: TaskTrace;
  updated_at: number;
};

export type TaskTree = {
  id: string;
  goal: string;
  tasks: Task[];
  created_at: number;
  updated_at: number;
  status: "running" | "done" | "failed" | "cancelled" | "timeout";
};

export type ProjectMap = {
  root: string;
  scanned_at: number;
  languages: string[];
  entry_points: string[];
  configs: string[];
  dependencies: string[];
  file_count: number;
  workspace?: boolean;
  scan_ms?: number;
  truncated?: boolean;
};

export type TaskGoalStarted = {
  id: string;
  goal: string;
  task_count: number;
  created_at: number;
};

export type TaskAddedEvent = {
  goal_id: string;
  task: Task;
};

export type TaskUpdateEvent = {
  goal_id: string;
  id: string;
  /**
   * Backend ships the task description on every update so the UI can
   * render a real label when a `task:update` arrives before the matching
   * `task:added` (late subscription, reloaded tree, etc.). Marked
   * optional for backwards-compat with older payloads.
   */
  description?: string;
  status: TaskStatus;
  retries?: number;
  result?: string | null;
  updated_at?: number;
  retries_bumped?: boolean;
};

export type TaskGoalDoneEvent = {
  id: string;
  goal: string;
  status: "running" | "done" | "failed" | "cancelled" | "timeout";
  completed: number;
  failed: number;
};

export type TaskFailureLoggedEvent = {
  task_id: string;
  error: string;
};

export type TaskCircuitTrippedEvent = {
  goal_id: string;
  consecutive_failures: number;
  threshold: number;
};

export type FailureLogEntry = {
  at: number;
  task_id: string;
  error: string;
};

export type StepStatus = "running" | "done" | "failed";

export type StepEvent = {
  index: number;
  role: AgentRole;
  title: string;
  status: StepStatus;
};

export type ConfirmRequest = {
  id: string;
  cmd: string;
  project_dir: string;
  timeout_ms: number;
};

export type ExecutionEvent =
  | { kind: "tool_call"; call: ToolCall; at: number }
  | { kind: "tool_result"; result: ToolResult; at: number }
  | { kind: "step"; step: StepEvent; at: number }
  | { kind: "info"; text: string; at: number }
  | { kind: "error"; text: string; role?: AgentRole; at: number };
