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

export type ToolCall = {
  id: string;
  name: string;
  args: unknown;
};

export type ToolResult = {
  id: string;
  ok: boolean;
  output: string;
  diff: string | null;
};

export type ChatMessage = {
  id: string;
  role: "user" | "assistant" | "system" | "tool";
  content: string;
  tool_calls?: ToolCall[];
  tool_results?: ToolResult[];
  streaming?: boolean;
};

export type Settings = {
  openrouter_api_key: string;
  openrouter_model: string;
  ollama_base_url: string;
  ollama_model: string;
};

export type ExecutionEvent =
  | { kind: "tool_call"; call: ToolCall; at: number }
  | { kind: "tool_result"; result: ToolResult; at: number }
  | { kind: "info"; text: string; at: number }
  | { kind: "error"; text: string; at: number };
