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
  /**
   * Provider actually routed for the agent that authored this message,
   * populated from the Phase 2 `ai:step` event. Present only on
   * assistant messages that came from a real model call.
   */
  provider?: "openrouter" | "ollama";
  /** Concrete model id used on the wire (e.g. `"openrouter/auto"`). */
  model?: string;
  /** Epoch ms when the first token streamed in. */
  started_at?: number;
  /** Epoch ms when streaming ended (or the bubble was synthesised). */
  ended_at?: number;
  /**
   * Optional discriminator for `role === "system"` bubbles. Lets the
   * renderer pick a tone and (for `"warn_action"`) wire up a click
   * handler to open Settings. Default (`undefined`) stays backwards
   * compatible with the pre-F-4 behaviour.
   */
  kind?: "info" | "warn_action";
};

/**
 * How agent roles (planner, executor, reviewer) map onto backends.
 *
 * - `cloud`  — every role runs on OpenRouter. No fallback.
 * - `local`  — every role runs on Ollama. No fallback.
 * - `hybrid` — planner + reviewer on OpenRouter, executor on Ollama;
 *   each role falls back to the other provider when the primary fails.
 */
export type ProviderMode = "cloud" | "local" | "hybrid";

export type Settings = {
  openrouter_api_key: string;
  openrouter_model: string;
  ollama_base_url: string;
  ollama_model: string;
  /**
   * Provider routing strategy. See {@link ProviderMode}. Defaults to
   * `hybrid` when an OpenRouter key is present, otherwise `local`.
   */
  provider_mode: ProviderMode;
  /**
   * Optional per-role model override. Empty means "use the default for
   * whichever provider routes this role" (`openrouter_model` or
   * `ollama_model`). A non-empty string is used verbatim.
   */
  planner_model: string;
  reviewer_model: string;
  executor_model: string;
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
  /**
   * If true, `send_chat` / autonomous task turns drop chat-history
   * messages older than {@link context_compaction_keep_last} before
   * calling the model. This is a plain sliding-window trim, not a
   * summary. Useful for small local models whose context fills up in
   * long sessions.
   */
  context_compaction_enabled: boolean;
  /**
   * How many of the most recent history messages to preserve when
   * {@link context_compaction_enabled} is true. Older messages are
   * dropped. Clamped at 2 by the backend.
   */
  context_compaction_keep_last: number;
  /**
   * OC-Titan §VI.2/§VI.3 test-mode gates. All three default to false
   * on the backend (`#[serde(default)]`) so they are optional on the
   * frontend type. When the user flips the dev-mode toggle, the UI
   * sets all three to true in a single `save_settings` call.
   */
  autoinstall_enabled?: boolean;
  security_gate_execute_enabled?: boolean;
  runtime_validation_enabled?: boolean;
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
  /**
   * Provider actually routed for this step. Emitted by the backend
   * dispatcher (`resolve_provider`). Absent on non-model steps (e.g.
   * synthesis bubbles emitted purely from local state).
   */
  provider?: "openrouter" | "ollama";
  /** Concrete model identifier used on the wire (e.g. `"openrouter/auto"`). */
  model?: string;
};

/**
 * OC-Titan self-healing pipeline roles (§VI.2 / §VI.3). These are
 * **disjoint** from the legacy `AgentRole` union (`planner` /
 * `executor` / `reviewer`) — the backend multiplexes both families of
 * events onto the same `ai:step` channel, and the frontend
 * discriminates by role membership.
 *
 * Emitted from:
 * - `controller.rs`  — guard / compiler / execution / runtime
 * - `autoinstall.rs` — autoinstall
 * - `run_cmd_gate.rs`— execution (run_cmd.policy)
 * - `ai.rs`          — codegen envelope lifecycle (role="executor",
 *                       covered by the legacy channel)
 */
export type PipelineRole =
  | "guard"
  | "compiler"
  | "execution"
  | "runtime"
  | "autoinstall"
  | "security";

export function isPipelineRole(role: unknown): role is PipelineRole {
  return (
    role === "guard" ||
    role === "compiler" ||
    role === "execution" ||
    role === "runtime" ||
    role === "autoinstall" ||
    role === "security"
  );
}

/** Status tags the Rust emitters use on `ai:step` payloads. */
export type PipelineStepStatus = "running" | "done" | "failed" | "warning";

/**
 * Discriminated subset of `ai:step` payloads emitted by the OC-Titan
 * pipeline (dependency guard → compiler gate → autoinstall →
 * execution → runtime validation). Intentionally permissive on
 * extras — the frontend only needs `{role, label, status, attempt?}`
 * for the tiered renderer; any extra fields are preserved as-is so
 * future backend additions don't require a frontend release.
 */
export type PipelineStepEvent = {
  role: PipelineRole;
  label: string;
  status: PipelineStepStatus;
  attempt?: number;
  reason?: string;
  missing?: string[];
  exit_code?: number;
  class?: "safe" | "warning" | "dangerous";
  [extra: string]: unknown;
};

/**
 * Six-state TaskPanel state machine driven by the pipeline event
 * stream. The transition function lives in `store.ts` and is purely
 * event-driven — no timers, no client-side retry counters.
 */
export type PipelinePhase =
  | "idle"
  | "running"
  | "waiting_confirm"
  | "retrying"
  | "completed"
  | "failed";

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
