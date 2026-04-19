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
};

export type TaskTree = {
  id: string;
  goal: string;
  tasks: Task[];
  created_at: number;
  updated_at: number;
  status: "running" | "done" | "failed" | "cancelled";
};

export type ProjectMap = {
  root: string;
  scanned_at: number;
  languages: string[];
  entry_points: string[];
  configs: string[];
  dependencies: string[];
  file_count: number;
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
  status: TaskStatus;
  retries?: number;
  result?: string | null;
  updated_at?: number;
  retries_bumped?: boolean;
};

export type TaskGoalDoneEvent = {
  id: string;
  goal: string;
  status: "running" | "done" | "failed" | "cancelled";
  completed: number;
  failed: number;
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
