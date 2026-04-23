/**
 * Global application store backed by Zustand (audit §7.6, plan §5.6).
 *
 * Before this module, `App.tsx` owned every piece of cross-cutting
 * state (projectDir, health probes, chat messages, event ring buffer,
 * sidebar toggles) via `useState` chains and then prop-drilled them
 * into leaf components. Any new feature that needed access —
 * e.g. a task-panel component that wanted to append an error entry to
 * the Debug log — had to plumb another prop down.
 *
 * This store centralises that state while keeping the same semantics
 * (event ring buffer, auto-open Debug on error). Components read
 * slices with `useAppStore(s => s.slice)` so only the bits they
 * actually consume cause re-renders.
 *
 * Keep this file free of React/DOM imports; it must remain testable
 * and renderer-process-compatible.
 */
import { create } from "zustand";
import type { AgentRole, ChatMessage, ExecutionEvent } from "./types";
import type {
  FailureLogEntry,
  PipelinePhase,
  PipelineStepEvent,
} from "./types";

/**
 * Maximum number of entries the in-memory execution-event ring buffer
 * keeps before oldest entries are dropped. Matches the cap previously
 * enforced inline in `App.tsx`.
 */
export const EVENTS_CAP = 500;
export const FAILURES_CAP = 50;

/**
 * OC-Titan §VI.2/§VI.3 — pipeline event ring-buffer cap. Sized to
 * comfortably cover a worst-case `max_compile_retries` budget (guard +
 * autoinstall + compiler + execution + runtime emissions per attempt)
 * without risking unbounded growth when the backend is chatty.
 */
export const PIPELINE_EVENTS_CAP = 200;

/**
 * LocalStorage key for the dev-mode toggle. The toggle is purely a
 * UI convenience — the backend gate booleans live in `Settings` and
 * are persisted through `save_settings`. We mirror the user's intent
 * locally so a fresh boot remembers whether dev-mode is "on" without
 * having to re-read backend settings to infer it.
 */
const DEV_MODE_STORAGE_KEY = "oc-titan:dev-mode";

function loadDevMode(): boolean {
  if (typeof window === "undefined") return false;
  try {
    return window.localStorage.getItem(DEV_MODE_STORAGE_KEY) === "1";
  } catch {
    return false;
  }
}

function persistDevMode(v: boolean): void {
  if (typeof window === "undefined") return;
  try {
    window.localStorage.setItem(DEV_MODE_STORAGE_KEY, v ? "1" : "0");
  } catch {
    // Private-mode / quota-exceeded: silently ignore, the in-memory
    // value still drives the UI for the current session.
  }
}

/**
 * Pure state-machine transition for `pipelinePhase`. Inputs are the
 * current phase and the latest `PipelineStepEvent`; output is the
 * next phase. No timers, no derived waiting heuristics — phase only
 * advances on real backend events.
 *
 * Explicit precedence (priority order, highest first):
 *
 *   failed > completed > waiting_confirm > retrying > running > hold
 *
 * The ordering matters when two events arrive close together — e.g.
 * `execution: run_cmd.started` (running) immediately followed by
 * `execution: run_cmd.confirmation` (waiting_confirm). We classify
 * the incoming event at each priority level and return the highest
 * one that matches. Unknown or non-classifying events leave the
 * phase unchanged.
 *
 * `completed` is intentionally gated on a **runtime-terminal** event
 * (`runtime.ok` on success, `runtime.skipped` when runtime validation
 * is disabled or execution never fired). We never treat `compiler.ok`
 * alone as terminal, because a subsequent `runtime.errors` would then
 * arrive **after** the UI had already declared success — which would
 * be user-hostile. The backend always emits one of the `runtime.*`
 * terminal labels to close a turn, so keying completion to that
 * family is safe.
 *
 * Deliberately conservative: once the phase reaches `completed` or
 * `failed`, it stays there for the rest of the turn. `resetPipeline()`
 * is the only path back to `idle`, and the caller (Chat / TaskPanel)
 * invokes it at the **start of a new turn**, never in response to
 * an individual event.
 */
export function nextPipelinePhase(
  current: PipelinePhase,
  event: PipelineStepEvent,
): PipelinePhase {
  if (current === "completed" || current === "failed") return current;

  const { role, label, status } = event;

  // --- Priority 1: failed (terminal failure) ---
  if (label.endsWith(".exhausted")) return "failed";
  if (
    role === "execution" &&
    (label === "run_cmd.refused" ||
      label === "run_cmd.blocked" ||
      label === "run_cmd.user_denied")
  ) {
    return "failed";
  }
  if (
    role === "autoinstall" &&
    (label === "autoinstall.refused" ||
      label === "autoinstall.blocked" ||
      label === "autoinstall.user_denied")
  ) {
    return "failed";
  }

  // --- Priority 2: completed (runtime-terminal success) ---
  //
  // `runtime.ok`      — runtime validation ran and returned exit 0.
  // `runtime.skipped` — runtime validation was skipped (runtime gate
  //                     off, or execution never fired). The backend
  //                     only emits this once per turn after the
  //                     pipeline has advanced past compiler gate, so
  //                     treating it as terminal is safe.
  //
  // `compiler.ok` on its own is *not* terminal — see docstring.
  if (role === "runtime" && (label === "runtime.ok" || label === "runtime.skipped")) {
    return "completed";
  }

  // --- Priority 3: waiting_confirm (blocked on user) ---
  if (role === "execution" && label === "run_cmd.confirmation") {
    return "waiting_confirm";
  }
  if (role === "autoinstall" && label === "autoinstall.attempting") {
    // `autoinstall.attempting` routes through the security gate;
    // with `warning_mode="prompt"` the backend opens a confirm modal.
    // The frontend can't distinguish `allow` vs `prompt` policy from
    // the payload alone, so we conservatively classify as
    // `waiting_confirm` and rely on the follow-up `run_cmd.started`
    // (running) / `run_cmd.user_denied` (failed) to move on.
    return "waiting_confirm";
  }

  // --- Priority 4: retrying ---
  if (label.endsWith(".retry")) return "retrying";
  if (role === "autoinstall" && label === "autoinstall.failed") {
    return "retrying";
  }

  // --- Priority 5: running ---
  if (status === "running") return "running";

  // --- No higher-priority match: hold. ---
  return current;
}

export type HealthStatus = boolean | null;

/**
 * Narrows the copy rendered in TaskPanel's pre-execution "planning…"
 * chip. `null` means no goal is currently in the planning phase.
 * Scenario-A §9.2 F-2: without this, the pane was silent for the
 * entire 2+ minute plan phase on small local models.
 */
export type GoalPlanningPhase = "scanning" | "planning" | null;

export interface AppState {
  // --- session identity ---
  projectDir: string | null;
  /** fs-change tick — incremented every time watcher fires a change. */
  fsTick: number;

  // --- health probes ---
  plannerOk: HealthStatus;
  executorOk: HealthStatus;

  // --- chat ---
  messages: ChatMessage[];

  // --- execution / debug log (ring-buffered) ---
  events: ExecutionEvent[];

  // --- failures log (project-scoped, capped) ---
  failures: FailureLogEntry[];

  // --- pre-execution planning chip (F-2) ---
  goalPlanning: GoalPlanningPhase;

  // --- OC-Titan §VI.2/§VI.3 pipeline slice ---
  /** Ring-buffered stream of OC-Titan pipeline events for the
   *  tiered renderer (ThinkingBlock / SystemAction / FinalAnswer). */
  pipelineEvents: PipelineStepEvent[];
  /** Current TaskPanel state-machine value; advanced purely by
   *  backend events via {@link nextPipelinePhase}. */
  pipelinePhase: PipelinePhase;
  /** Latest `attempt` counter seen on any pipeline event, for the
   *  TaskPanel retry chip. Sticks at the highest-seen value within
   *  a turn; reset by {@link resetPipeline}. */
  pipelineAttempt: number;
  /** Most recent `label` seen, used to render a one-line status chip
   *  in TaskPanel ("autoinstall.attempting", "compiler.running", …). */
  pipelineLastLabel: string | null;
  /** Local UI toggle — when true, the Settings save path flips all
   *  three backend gate booleans on simultaneously. Persisted to
   *  `localStorage`; not round-tripped through the backend. */
  devMode: boolean;

  // --- UI toggles ---
  debugOpen: boolean;
  explorerOpen: boolean;
  settingsOpen: boolean;
  bottomPanelHeight: number;

  // --- actions ---
  setProjectDir: (dir: string | null) => void;
  bumpFsTick: () => void;
  setPlannerOk: (ok: HealthStatus) => void;
  setExecutorOk: (ok: HealthStatus) => void;

  setGoalPlanning: (phase: GoalPlanningPhase) => void;

  setMessages: (
    updater: ChatMessage[] | ((prev: ChatMessage[]) => ChatMessage[]),
  ) => void;
  resetMessages: () => void;

  /** Append to the event log with ring-buffer trimming. */
  pushEvent: (e: ExecutionEvent) => void;
  /** Append an error and auto-open the Debug panel. */
  pushError: (text: string, role?: AgentRole) => void;
  replaceEvents: (events: ExecutionEvent[]) => void;
  clearEvents: () => void;

  setFailures: (failures: FailureLogEntry[]) => void;
  pushFailure: (f: FailureLogEntry) => void;
  clearFailures: () => void;

  /** Append an OC-Titan pipeline event (ring-buffered, advances the
   *  phase state machine). */
  pushPipelineEvent: (e: PipelineStepEvent) => void;
  /** Clear the pipeline slice back to `idle` — called on new turn. */
  resetPipeline: () => void;
  /** Flip the dev-mode toggle. Persists to `localStorage`; the actual
   *  backend gate booleans are saved by the Settings dialog. */
  setDevMode: (v: boolean) => void;

  setDebugOpen: (v: boolean) => void;
  setExplorerOpen: (v: boolean) => void;
  toggleDebug: () => void;
  toggleExplorer: () => void;
  setSettingsOpen: (v: boolean) => void;
  setBottomPanelHeight: (v: number) => void;
}

/**
 * The single application store. Keep state access scoped with
 * selectors in consumers to avoid spurious re-renders — e.g.
 * `useAppStore(s => s.messages)` not `useAppStore()`.
 */
export const useAppStore = create<AppState>((set) => ({
  projectDir: null,
  fsTick: 0,
  plannerOk: null,
  executorOk: null,
  messages: [],
  events: [],
  failures: [],
  goalPlanning: null,
  pipelineEvents: [],
  pipelinePhase: "idle",
  pipelineAttempt: 0,
  pipelineLastLabel: null,
  devMode: loadDevMode(),
  debugOpen: false,
  explorerOpen: true,
  settingsOpen: false,
  bottomPanelHeight: 240,

  setProjectDir: (dir) => set({ projectDir: dir }),
  bumpFsTick: () => set((s) => ({ fsTick: s.fsTick + 1 })),
  setPlannerOk: (ok) => set({ plannerOk: ok }),
  setExecutorOk: (ok) => set({ executorOk: ok }),

  setGoalPlanning: (phase) => set({ goalPlanning: phase }),

  setMessages: (updater) =>
    set((s) => ({
      messages:
        typeof updater === "function"
          ? (updater as (prev: ChatMessage[]) => ChatMessage[])(s.messages)
          : updater,
    })),
  resetMessages: () => set({ messages: [] }),

  pushEvent: (e) =>
    set((s) => {
      const trimmed =
        s.events.length >= EVENTS_CAP
          ? s.events.slice(-(EVENTS_CAP - 1))
          : s.events;
      return { events: [...trimmed, e] };
    }),
  pushError: (text, role) =>
    set((s) => {
      const entry: ExecutionEvent = {
        kind: "error",
        text,
        role,
        at: Date.now(),
      };
      const trimmed =
        s.events.length >= EVENTS_CAP
          ? s.events.slice(-(EVENTS_CAP - 1))
          : s.events;
      return {
        events: [...trimmed, entry],
        // Silent collapsed Debug on a failed run is exactly the UX
        // the audit flagged — pop it open the moment something fails.
        debugOpen: true,
      };
    }),
  replaceEvents: (events) => set({ events }),
  clearEvents: () => set({ events: [] }),

  setFailures: (failures) =>
    set({ failures: failures.slice(-FAILURES_CAP).sort((a, b) => b.at - a.at) }),
  pushFailure: (f) =>
    set((s) => ({
      failures: [f, ...s.failures].slice(0, FAILURES_CAP),
      debugOpen: true,
    })),
  clearFailures: () => set({ failures: [] }),

  pushPipelineEvent: (e) =>
    set((s) => {
      const trimmed =
        s.pipelineEvents.length >= PIPELINE_EVENTS_CAP
          ? s.pipelineEvents.slice(-(PIPELINE_EVENTS_CAP - 1))
          : s.pipelineEvents;
      const attempt =
        typeof e.attempt === "number" && e.attempt > s.pipelineAttempt
          ? e.attempt
          : s.pipelineAttempt;
      return {
        pipelineEvents: [...trimmed, e],
        pipelinePhase: nextPipelinePhase(s.pipelinePhase, e),
        pipelineAttempt: attempt,
        pipelineLastLabel: e.label,
      };
    }),
  resetPipeline: () =>
    set({
      pipelineEvents: [],
      pipelinePhase: "idle",
      pipelineAttempt: 0,
      pipelineLastLabel: null,
    }),
  setDevMode: (v) => {
    persistDevMode(v);
    set({ devMode: v });
  },

  setDebugOpen: (v) => set({ debugOpen: v }),
  setExplorerOpen: (v) => set({ explorerOpen: v }),
  toggleDebug: () => set((s) => ({ debugOpen: !s.debugOpen })),
  toggleExplorer: () => set((s) => ({ explorerOpen: !s.explorerOpen })),
  setSettingsOpen: (v) => set({ settingsOpen: v }),
  setBottomPanelHeight: (v) => set({ bottomPanelHeight: v }),
}));
