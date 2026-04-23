# PR-N Test Plan — OC-Titan §VI.2/§VI.3 UI tiers + TaskPanel state machine

PR: https://github.com/hadyksa2025-ui/aiservx-claude-code-main/pull/14 (merged)
Session: https://app.devin.ai/sessions/18adab956ee84cb2be90a0bc3a51371d

## What changed (user-visible)

PR-N is a strict thin-renderer over `ai:step` backend events that the OC-Titan
pipeline (PR-A … PR-M) already emits. Six user-visible additions:

1. **Pipeline store slice** (`store.ts`): ring-buffered `pipelineEvents` (cap 200),
   `pipelinePhase` 6-state machine, `pipelineAttempt`, `pipelineLastLabel`, `devMode`.
2. **`nextPipelinePhase()` explicit precedence**
   (<ref_snippet file="/home/ubuntu/aiservx-claude-code-main/desktop/frontend/src/store.ts" lines="103-169" />):
   `failed > completed > waiting_confirm > retrying > running > hold`.
3. **Runtime-guarded completion**: `completed` fires **only** on `runtime.ok` or
   `runtime.skipped` — `compiler.ok` alone is *not* terminal
   (<ref_snippet file="/home/ubuntu/aiservx-claude-code-main/desktop/frontend/src/store.ts" lines="130-142" />).
4. **`PipelineMessageList`** (new component): Tier-2 block rows for `guard` /
   `compiler` / `security`, Tier-3 `SystemAction` pills for `autoinstall` /
   `execution` / `runtime`
   (<ref_snippet file="/home/ubuntu/aiservx-claude-code-main/desktop/frontend/src/components/PipelineMessageList.tsx" lines="39-43" />).
5. **TaskPanel pipeline chip** — visible only when `pipelinePhase !== "idle"`,
   shows `pipeline: <phase>` + optional `· attempt N` + optional `· <last label>`
   (<ref_snippet file="/home/ubuntu/aiservx-claude-code-main/desktop/frontend/src/components/TaskPanel.tsx" lines="317-340" />).
6. **Dev-mode toggle** in Settings — one checkbox flips three backend gates
   (`autoinstall_enabled`, `security_gate_execute_enabled`,
   `runtime_validation_enabled`) in lockstep; `dependency_guard_enabled` stays at
   default-on. Persists in localStorage key `oc-titan:dev-mode`
   (<ref_snippet file="/home/ubuntu/aiservx-claude-code-main/desktop/frontend/src/components/Settings.tsx" lines="1202-1240" />).
7. **Turn-start reset** — `resetPipeline()` called from `Chat.send()` and
   `TaskPanel.startGoal()` before any event work
   (<ref_snippet file="/home/ubuntu/aiservx-claude-code-main/desktop/frontend/src/components/Chat.tsx" lines="256-258" />
   and <ref_snippet file="/home/ubuntu/aiservx-claude-code-main/desktop/frontend/src/components/TaskPanel.tsx" lines="201-211" />).

## Testing strategy

A real end-to-end run of the backend self-healing pipeline would require (a) a
live Tauri desktop build, (b) an LLM capable of emitting a valid codegen
envelope, (c) a scratch project with missing deps. That is high-cost and
non-deterministic, and the PR is explicitly a *thin renderer* — the adversarial
question is **"does the UI render pipeline events correctly?"**, not "does the
backend still work?" (the 191 Rust tests already assert that).

The deterministic path:

- **Vite dev server** serves the React frontend at `http://localhost:5173`.
- A **temporary test-only scaffold** (reverted after testing; not committed)
  exposes `window.__store = useAppStore` and stubs `@tauri-apps/api`'s
  `invoke`/`listen` so `Settings` and `App` mount cleanly without Tauri.
- We drive synthetic `pushPipelineEvent(...)` calls from the browser devtools
  console. Since `App.tsx`'s real `ai:step` listener calls exactly the same
  `pushPipelineEvent` on a matching payload, these calls are indistinguishable
  from a real backend emit for every downstream claim.

The scaffold only injects a bridge and stub; it never alters `store.ts`,
`PipelineMessageList.tsx`, `Settings.tsx`, `TaskPanel.tsx`, or `Chat.tsx` —
the code-under-test is unmodified.

## Test cases

### TC-1 — State machine precedence is **explicit**, not first-match

**Why it matters:** the user's explicit refinement was
*"waiting_confirm must beat running whenever two events arrive close together"*.
A naive first-match implementation would leave phase on `running` if
`execution.started` arrived before `execution.confirmation` within the same tick.

**Steps (devtools console, via `__store.getState()`):**

1. `resetPipeline()` — baseline `phase === "idle"`.
2. `pushPipelineEvent({ role: "execution", label: "run_cmd.started", status: "running" })`.
   **Expected:** `pipelinePhase === "running"`. TaskPanel chip shows
   `pipeline: running` with class `task-pipeline-chip-running`.
3. `pushPipelineEvent({ role: "execution", label: "run_cmd.confirmation", status: "running" })`.
   **Expected:** `pipelinePhase === "waiting_confirm"`. Chip text now reads
   `pipeline: waiting confirm` (underscore replaced by space — see
   `TaskPanel.tsx:329`).
4. `pushPipelineEvent({ role: "execution", label: "run_cmd.started", status: "running" })`
   (another running event after waiting_confirm).
   **Expected:** `pipelinePhase` **stays** `"waiting_confirm"` (current phase is
   not `completed`/`failed`, but the new event only matches priority-5 "running",
   which is lower than current phase: by design of "hold" behaviour the phase
   should not move backwards. Actually — reading the code at
   <ref_snippet file="/home/ubuntu/aiservx-claude-code-main/desktop/frontend/src/store.ts" lines="164-168" /> —
   `status === "running"` would overwrite. We'll measure the actual behaviour
   and flag if it regresses waiting_confirm → running silently.)

**Pass/fail criteria:**
- Step 3 must leave `pipelinePhase === "waiting_confirm"` — NOT `"running"`.
  A broken first-match transition would fail this assertion visibly.
- Chip text at step 3: **exactly** `"pipeline: waiting confirm"` (note space, not
  underscore).

### TC-2 — Runtime-guarded completion (`compiler.ok` alone ≠ terminal)

**Why it matters:** the user's explicit refinement was
*"لو runtime enabled → ماينفعش تعتبر compiler.ok نهاية ... completed فقط من runtime.ok"*.
A broken implementation would set `completed` on `compiler.ok` and a subsequent
`runtime.errors` would arrive **after** the UI declared success.

**Steps (devtools console):**

1. `resetPipeline()`.
2. `pushPipelineEvent({ role: "compiler", label: "compiler.ok", status: "done" })`.
   **Expected:** `pipelinePhase === "running"` (status=running would move; but
   status=done with no priority-1/2/3/4 match falls through — actual expected is
   that the current phase is held since no priority classifies it; if current is
   `idle`, it stays `idle` after `compiler.ok`. Read the transition fn: `done`
   status does not match priority 5 "running". So phase stays `idle`.)
3. **Concrete assertion:** after step 2, `pipelinePhase !== "completed"`. This
   is the critical adversarial bit — a broken guard would return `"completed"`
   from `compiler.ok`.
4. `pushPipelineEvent({ role: "runtime", label: "runtime.ok", status: "done" })`.
   **Expected:** `pipelinePhase === "completed"` **now** (not before).
   TaskPanel chip class becomes `task-pipeline-chip-completed`, label reads
   `pipeline: completed`.
5. Additional adversarial follow-up (sticky-terminal):
   `pushPipelineEvent({ role: "compiler", label: "compiler.errors", status: "failed" })`.
   **Expected:** `pipelinePhase` stays `"completed"` (terminal phases are
   sticky within a turn per docstring).

**Pass/fail criteria:**
- After step 2 (compiler.ok but no runtime event yet),
  `pipelinePhase !== "completed"` — a broken runtime-guard would fail this.
- After step 4, `pipelinePhase === "completed"` — proves the runtime terminal
  does advance the phase.
- After step 5, `pipelinePhase === "completed"` still — proves terminal stickiness.

### TC-3 — Failed takes priority over everything (`*.exhausted`, refused)

**Steps:**

1. `resetPipeline()`, drive to `running` via a `status: "running"` event.
2. `pushPipelineEvent({ role: "runtime", label: "runtime.exhausted", status: "failed" })`.
   **Expected:** `pipelinePhase === "failed"`.
3. `pushPipelineEvent({ role: "runtime", label: "runtime.ok", status: "done" })`.
   **Expected:** `pipelinePhase` stays `"failed"` (sticky terminal per code
   <ref_snippet file="/home/ubuntu/aiservx-claude-code-main/desktop/frontend/src/store.ts" lines="107-107" />).
4. `resetPipeline()`, then
   `pushPipelineEvent({ role: "execution", label: "run_cmd.refused", status: "failed" })`.
   **Expected:** `pipelinePhase === "failed"`.

**Pass/fail criteria:** chip class must become `task-pipeline-chip-failed` and
must not flip away on any subsequent event until `resetPipeline()` is called.

### TC-4 — 3-tier rendering (Tier-2 block row vs Tier-3 pill)

**Why it matters:** PR claims roles partition into block rows and inline pills.
A broken classifier would render all events as one style.

**Steps (devtools console):**

1. `resetPipeline()`.
2. Push a Tier-2 event:
   `pushPipelineEvent({ role: "guard", label: "dependency.missing", status: "warning", missing: ["react", "zustand"] })`.
   **Expected (visual):** A new row under Chat transcript carries class
   `pipeline-tier-2 pipeline-role-guard pipeline-status-warning`. Chip text:
   `Dependency guard`. Status glyph: `!`. Main text contains
   `dependency.missing — missing: react, zustand`.
3. Push a Tier-3 event:
   `pushPipelineEvent({ role: "autoinstall", label: "autoinstall.attempting", status: "running", attempt: 1 })`.
   **Expected (visual):** New row carries class `pipeline-tier-3`, contains a
   `<span class="pipeline-role-prefix">Auto-install:</span>` and a
   `SystemAction` pill with glyph `⏵`, tone `info`, and text
   `autoinstall.attempting — attempt 1`.
4. Push a Tier-3 event with failed status:
   `pushPipelineEvent({ role: "runtime", label: "runtime.errors", status: "failed", exit_code: 1 })`.
   **Expected (visual):** Tier-3 row with `SystemAction` tone `error`, glyph `✗`,
   text `runtime.errors — exit 1`.
5. Push a Tier-2 event with done status:
   `pushPipelineEvent({ role: "compiler", label: "compiler.ok", status: "done" })`.
   **Expected:** Tier-2 row with chip `Compiler gate`, glyph `✓`.

**Pass/fail criteria (visual + DOM inspect):**
- Tier-2 rows must have `class` containing `pipeline-tier-2` AND NOT
  `pipeline-tier-3`.
- Tier-3 rows must have `class` containing `pipeline-tier-3` AND NOT
  `pipeline-tier-2`.
- Block-row chip text must match `roleLabel()` mapping:
  `guard → "Dependency guard"`, `compiler → "Compiler gate"`,
  `security → "Security classifier"`.
- Pill tone colour must match status: `running/info`, `done/success`,
  `warning/warn`, `failed/error`.

### TC-5 — Dev-mode toggle: three-gate lockstep + `dependency_guard` untouched + persistence + tooltip

**Why it matters:** user refinements #3 and #5 explicitly required
(a) tooltip listing three values, (b) dependency_guard NOT flipped.

**Steps (Vite UI):**

1. Click Settings icon to open `SettingsModal`. Scroll to the dev-mode row.
2. Hover the `Dev-mode: enable OC-Titan self-healing pipeline…` label.
   **Expected:** tooltip text is **exactly**
   `execution: OFF · runtime: OFF · autoinstall: OFF — enable to see the self-healing pipeline end-to-end`
   (from <ref_snippet file="/home/ubuntu/aiservx-claude-code-main/desktop/frontend/src/components/Settings.tsx" lines="1204-1208" />).
3. Read the subdued caption line under the checkbox:
   **Expected:** `execution: OFF · runtime: OFF · autoinstall: OFF · dependency_guard: default`.
4. Click the checkbox to ON.
   **Expected (in-memory):**
   - `__store.getState().devMode === true`.
   - `localStorage.getItem("oc-titan:dev-mode") === "1"`.
   - The in-dialog `s` state mutated to set the three flags true (verify by
     poking a React DevTools expression or by clicking Save and then reopening
     Settings).
5. Hover the label again.
   **Expected tooltip:** `execution: ON · runtime: ON · autoinstall: ON`.
6. Read caption.
   **Expected:** `execution: ON · runtime: ON · autoinstall: ON · dependency_guard: default`.
7. Close and reopen Settings.
   **Expected:** checkbox remains ON (localStorage persistence).
8. Explicitly probe `dependency_guard_enabled` handling:
   **Expected:** the `Settings` type has no `dependency_guard_enabled` field in
   the frontend (verified by searching types.ts: only `autoinstall_enabled`,
   `security_gate_execute_enabled`, `runtime_validation_enabled` are added) —
   so the toggle has no path to mutate `dependency_guard_enabled` on the
   backend settings blob. The caption literal `dependency_guard: default`
   reflects this intent. **Pass** if both are true.
9. Uncheck the dev-mode box.
   **Expected:** caption flips back to `… OFF … OFF … OFF …`, localStorage
   becomes `"0"`, `__store.getState().devMode === false`.

**Pass/fail criteria:**
- Caption literal text must match exactly (no emoji, no different separator).
- Tooltip literal text must match exactly.
- `localStorage["oc-titan:dev-mode"]` must survive a page reload.
- `types.ts` must not contain `dependency_guard_enabled` (grep).

### TC-6 — Turn-start reset (not first-event reset)

**Why it matters:** user refinement #4 —
*"عند بداية turn جديد (مش أول event)"*. A broken implementation that resets on
first event instead of turn-start would leak the last event of the previous
turn into the new turn's log.

**Steps (devtools console, simulating two turns):**

1. First "turn": push a sequence ending at `runtime.ok`.
   Verify `pipelineEvents.length > 0` and `pipelinePhase === "completed"`.
2. Directly call `__store.getState().resetPipeline()` (this is what
   `Chat.send()` does at `Chat.tsx:258`, verified by code inspection).
   **Expected:** `pipelineEvents.length === 0`, `pipelinePhase === "idle"`,
   `pipelineAttempt === 0`, `pipelineLastLabel === null`. TaskPanel chip and
   `PipelineMessageList` both become empty / hidden.
3. Push a new event in the "second turn".
   **Expected:** `pipelineEvents.length === 1` (not 1+N previous). Chip
   reappears only from this event.
4. **Adversarial step:** before step 2 the chip showed `completed`. If we
   instead *push* a new event first (without resetting), the transition fn
   says terminal is sticky — so a broken "reset on first event" implementation
   would forever see `pipelinePhase === "completed"` on the new turn. By
   driving via the observed code path (resetPipeline → push), we prove the
   correct ordering.

**Pass/fail criteria:**
- After `resetPipeline()`, `pipelineEvents` is length 0 AND `pipelinePhase`
  is `"idle"`.
- After a fresh push, `pipelinePhase` reflects only the new event.

### TC-7 — Attempt counter monotonicity

**Why it matters:** PR claims `pipelineAttempt` sticks at highest-seen value
within a turn and resets with `resetPipeline()`
(<ref_snippet file="/home/ubuntu/aiservx-claude-code-main/desktop/frontend/src/store.ts" lines="354-357" />).

**Steps:**
1. `resetPipeline()`.
2. Push event with `attempt: 1` → expect `pipelineAttempt === 1`, chip shows
   `· attempt 1`.
3. Push event with `attempt: 3` → expect `pipelineAttempt === 3`,
   chip shows `· attempt 3`.
4. Push event with `attempt: 2` (a later event with a smaller value —
   hypothetical out-of-order arrival) → expect `pipelineAttempt === 3` still.
5. Push event with no attempt → expect `pipelineAttempt === 3` still.
6. `resetPipeline()` → expect `pipelineAttempt === 0` and chip hides the
   attempt span.

**Pass/fail criteria:** `pipelineAttempt` monotonic within a turn; resets on
turn-start; chip renders `· attempt N` only when `N > 0`.

## What is NOT tested (and why)

- **Real Rust backend → Tauri → frontend wiring.** That is validated by
  (a) the PR-A…PR-M Rust test suite (191/191) and (b) code inspection of the
  single `onEvent<"ai:step">` routing in `App.tsx:101-118` which calls exactly
  the same `pushPipelineEvent` we drive in tests.
- **Live LLM-driven pipeline end-to-end.** Non-deterministic, LLM-cost-dependent,
  and doesn't add signal beyond the synthetic event drives that exercise every
  branch of `nextPipelinePhase()`.
- **`Chat.send()` / `TaskPanel.startGoal()` Tauri invoke paths.** They require
  the Tauri host. Their reset side-effect is verified by reading the exact
  call-site and driving `resetPipeline()` directly.

## Pre-flight cleanup

- Revert the temporary `window.__store` bridge and `invoke`/`listen` stubs
  before reporting. The final VM state must have no uncommitted modifications
  to PR-N code.
