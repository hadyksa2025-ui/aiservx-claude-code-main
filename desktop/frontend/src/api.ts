import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import type {
  AgentRole,
  ChatMessage,
  ConfirmRequest,
  FsChange,
  FsEntry,
  ProjectMap,
  Settings,
  StepEvent,
  TaskTree,
  ToolCall,
  ToolResult,
} from "./types";

/** Thin wrapper over Tauri `invoke` with typed commands. */
export const api = {
  listDir: (project_dir: string, sub_path: string) =>
    invoke<FsEntry[]>("list_dir", { projectDir: project_dir, subPath: sub_path }),

  readFile: (project_dir: string, sub_path: string) =>
    invoke<string>("read_file", { projectDir: project_dir, subPath: sub_path }),

  writeFile: (project_dir: string, sub_path: string, content: string) =>
    invoke<string>("write_file", {
      projectDir: project_dir,
      subPath: sub_path,
      content,
    }),

  watchDir: (project_dir: string) =>
    invoke<void>("watch_dir", { projectDir: project_dir }),

  unwatchDir: (project_dir: string) =>
    invoke<void>("unwatch_dir", { projectDir: project_dir }),

  runCmd: (project_dir: string, cmd: string, timeout_ms?: number) =>
    invoke<{ stdout: string; stderr: string; exit_code: number }>("run_cmd", {
      projectDir: project_dir,
      cmd,
      timeoutMs: timeout_ms ?? 30000,
    }),

  sendChat: (
    project_dir: string,
    message: string,
    history: ChatMessage[],
  ): Promise<{
    assistant: string;
    tool_calls: ToolCall[];
    tool_results: ToolResult[];
    steps: StepEvent[];
  }> => invoke("send_chat", { projectDir: project_dir, message, history }),

  cancelChat: () => invoke<void>("cancel_chat"),

  getSettings: () => invoke<Settings>("get_settings"),
  saveSettings: (settings: Settings) =>
    invoke<void>("save_settings", { settings }),

  checkPlanner: () => invoke<boolean>("check_planner"),
  checkExecutor: () => invoke<boolean>("check_executor"),

  /** Resolve a pending `ai:confirm_request` (run_cmd safety gate). */
  confirmCmd: (id: string, approved: boolean) =>
    invoke<void>("confirm_cmd", { id, approved }),

  /** Start the autonomous task engine on a high-level user goal. */
  startGoal: (
    project_dir: string,
    goal: string,
  ): Promise<{
    goal_id: string;
    status: string;
    completed: number;
    failed: number;
  }> => invoke("start_goal", { projectDir: project_dir, goal }),

  /** Cooperatively cancel the top-level goal loop. */
  cancelGoal: () => invoke<void>("cancel_goal"),

  /** Scan the opened project and persist the resulting project_map. */
  scanProject: (project_dir: string) =>
    invoke<ProjectMap>("scan_project_cmd", { projectDir: project_dir }),

  /** Load the most-recently-persisted active task tree, if any. */
  loadTaskTree: (project_dir: string) =>
    invoke<TaskTree | null>("load_task_tree", { projectDir: project_dir }),
};

export type BackendEvent =
  | "ai:token"
  | "ai:tool_call"
  | "ai:tool_result"
  | "ai:step"
  | "ai:done"
  | "ai:error"
  | "ai:confirm_request"
  | "fs:changed"
  | "task:goal_started"
  | "task:added"
  | "task:update"
  | "task:goal_done"
  | "task:failure_logged"
  | "project:scan_done";

/** Listen to a backend event. Returns an unlisten function. */
export async function onEvent<T>(
  name: BackendEvent,
  handler: (payload: T) => void,
): Promise<UnlistenFn> {
  return listen<T>(name, (e) => handler(e.payload));
}

export type FsChangeEvent = FsChange;

export type TokenEvent = { text: string; role: AgentRole };
export type DoneEvent = { assistant: string; iterations: number };
export type ErrorEvent = { message: string; role?: AgentRole };
export type ConfirmEvent = ConfirmRequest;
