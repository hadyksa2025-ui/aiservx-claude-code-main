import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import type {
  ChatMessage,
  FsChange,
  FsEntry,
  Settings,
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
  ): Promise<{ assistant: string; tool_calls: ToolCall[]; tool_results: ToolResult[] }> =>
    invoke("send_chat", { projectDir: project_dir, message, history }),

  cancelChat: () => invoke<void>("cancel_chat"),

  getSettings: () => invoke<Settings>("get_settings"),
  saveSettings: (settings: Settings) =>
    invoke<void>("save_settings", { settings }),

  checkPlanner: () => invoke<boolean>("check_planner"),
  checkExecutor: () => invoke<boolean>("check_executor"),
};

/** Listen to a backend event. Returns an unlisten function. */
export async function onEvent<T>(
  name:
    | "ai:token"
    | "ai:tool_call"
    | "ai:tool_result"
    | "ai:done"
    | "ai:error"
    | "fs:changed",
  handler: (payload: T) => void,
): Promise<UnlistenFn> {
  return listen<T>(name, (e) => handler(e.payload));
}

export type FsChangeEvent = FsChange;
