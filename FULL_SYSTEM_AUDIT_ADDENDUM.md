# Full System Audit — Addendum & Verification

> **Scope**: Companion to `FULL_SYSTEM_AUDIT.md` (1024 lines, already in repo).
> **Purpose**: (1) verify the existing audit against the *current* code on
> `main`, (2) add findings not captured in the existing document,
> (3) provide a concise Arabic/English executive summary that the
> repository owner can act on.
> **Mode**: analysis only — no code changes, no PR.
> **Repo commit**: `main` at session time (April 2026).

---

## 0. Executive Summary (English)

The existing `FULL_SYSTEM_AUDIT.md` is **accurate and comprehensive**.
Every major claim I spot-checked — sandbox logic in `fs_ops::resolve`,
the hardcoded `Planner → OpenRouter / Executor → Ollama` routing in
`ai::run_chat_turn`, the empty per-task history in `controller::execute_task_with_retries`,
the `MAX_REVIEWER_RETRIES = 1` cap, the 600 s / 7 200 s timeouts, the
`project_context_summary` injection, the atomic PROJECT_MEMORY writes,
the 4-pane flat layout in `App.tsx`, the `Math.random()` uid in
`Chat.tsx` — was verified against the actual files.

The 5.6/10 composite maturity score is realistic. The 5-phase roadmap
(Core stability → Provider routing → Thinking UI → Task panel → Polish)
is sound. Estimated 5–8 weeks to a shippable v1.0 is believable if a
single focused engineer owns it.

This addendum adds **9 findings** the main audit either missed or
under-weighted, mostly around provider client configuration, health
probes, async/sync mutex hazards, and UX accessibility. None of them
change the composite verdict; they sharpen Phase 1 / Phase 2.

## 0. الملخص التنفيذي (عربي)

**الخلاصة**: ملف `FULL_SYSTEM_AUDIT.md` الموجود في الريبو دقيق وشامل،
والنقاط المذكورة فيه موثّقة فعلياً في الكود. النتيجة العامة **5.6/10**
معقولة، والنظام **ليس جاهزاً للإطلاق العام** لكنّه **صالح للاستخدام
المحلي المراقب لمستخدم واحد**.

**أهم ٤ عوائق لا تزال قائمة** (مرتبة بالأولوية):

1. **لا يوجد توجيه متعدد المزودين قابل للإعداد.** المسار `planner → OpenRouter / executor → Ollama`
   مدفون في الكود بدون `provider_mode` ولا fallback حقيقي
   (`call_executor_with_fallback` في `ai.rs:573-584` وعده بـ fallback لكنه
   لا يوفره). هذا هو العائق رقم 1.

2. **الـ Reviewer لا يرى نتائج الأدوات الفعلية.** في
   `controller::review_task` تمرَّر فقط `executor_summary` (نص) للمراجع،
   ولا تمرَّر له سجلات `tool_call/tool_result`. معناها: executor مهلوس
   يمكنه أن يدّعي كتابة ملف لم يُكتب، ويمرّ من المراجعة.

3. **لا يوجد ضغط/اختصار للسياق (context compaction).** الجلسات الطويلة
   ستتجاوز نافذة الموديل وتفشل بدون إنذار. الحل في Phase 1: نافذة
   منزلقة (آخر N دور مع الرسائل السيستمية).

4. **واجهة المستخدم مستوى developer، ليست مستوى product.** لا يوجد
   `ThinkingBlock` قابل للطي، لا توجد تفرقة بصرية بين
   reasoning وfinal answer، الـ execution timeline مسطّح بدون تجميع
   لكل task.

**التوصية**: نفّذ Phase 1 (استقرار) ثم Phase 2 (توجيه المزودين) قبل
أي عمل على الـ UI. بدون Phase 1+2 أي تحسين UI سيُبنى فوق أساس هشّ.

---

## 1. Verification Pass — Claims Cross-Checked

| # | Claim in main audit | Verified in code | Status |
|---|--------------------|------------------|--------|
| 1 | `call_executor_with_fallback` has no actual fallback | `ai.rs:573-584` — body is literally a single `stream_ollama` call | ✓ confirmed |
| 2 | Planner disabled when OpenRouter key empty | `ai.rs:767` `let use_planner = !settings.openrouter_api_key.is_empty();` | ✓ confirmed |
| 3 | No context compaction — full history pushed | `ai.rs:259-290` `build_executor_messages` iterates entire `history` with no windowing | ✓ confirmed |
| 4 | MAX_REVIEWER_RETRIES hardcoded | `ai.rs:51` `const MAX_REVIEWER_RETRIES: usize = 1;` | ✓ confirmed |
| 5 | Goal planner routes through full `run_chat_turn` | `controller.rs:682-690` calls `ai::run_chat_turn` with JSON-style prompt | ✓ confirmed |
| 6 | No JSON repair/retry in `plan_goal` | `controller.rs:692` returns error on parse fail; caller falls back to heuristic | ✓ confirmed |
| 7 | `heuristic_split_goal` English-only separators | `controller.rs:775` `separators = ["\n", ";", " and then ", " then "];` | ✓ confirmed |
| 8 | Each task starts with empty history | `controller.rs:454` `Vec::<UiMessage>::new()` | ✓ confirmed |
| 9 | `TaskStatus::Skipped` conflates cancel + dep-block | `tasks.rs:34` enum has single `Skipped`; controller and finalizer both write it | ✓ confirmed |
| 10 | `fs_ops::resolve` canonicalizes symlinks | `fs_ops.rs:36` `joined.canonicalize()` (resolves symlinks to target) | ✓ confirmed |
| 11 | `write_file` auto-creates parent dirs | `fs_ops.rs:145-147` `fs::create_dir_all(parent)` before the write | ✓ confirmed |
| 12 | Hidden-dir skip-list hardcoded | `fs_ops.rs:103-108` `matches!(name, ".git" \| "node_modules" \| ...)` | ✓ confirmed |
| 13 | `AppState.settings` is `Mutex`, not `RwLock` | `lib.rs:39` `pub settings: Mutex<Settings>` | ✓ confirmed |
| 14 | `events` array grows unbounded in App.tsx | `App.tsx:25,50-81` all `setEvents((prev) => [...prev, ...])`, no cap | ✓ confirmed |
| 15 | `uid()` uses `Math.random()` | `Chat.tsx:12-14` | ✓ confirmed |
| 16 | Atomic memory writes via tmp + rename | `memory.rs:74-84` | ✓ confirmed |
| 17 | Schema v2, 4 MB cap | `memory.rs:22-26` | ✓ confirmed |
| 18 | Trace bounded, per-field cap 4 KiB | `trace.rs:25-30` MAX_ENTRIES=200, MAX_TEXT_CHARS=4096 | ✓ confirmed |
| 19 | Project context injection into all 3 roles | `ai.rs:775-777` loaded once, passed to planner/executor/reviewer message builders | ✓ confirmed |
| 20 | 600 s task / 7 200 s goal defaults | `settings.rs:87-98` | ✓ confirmed |
| 21 | Circuit breaker 5 failures | `settings.rs:103-105` `default_circuit_breaker_threshold = 5` | ✓ confirmed |

**Verdict**: 21/21 spot-checked claims hold. The existing audit is
grounded in reality. The maturity score and Phase 1–5 roadmap stand.

---

## 2. New Findings (Not in Main Audit)

### 2.1 Provider client timeouts are asymmetric and under-documented

**Severity: Medium**

- OpenRouter SSE client: `reqwest` timeout **180 s** (`ai.rs:397-400`).
- Ollama SSE client: `reqwest` timeout **300 s** (`ai.rs:506-509`).
- Per-task wall timeout (settings): **600 s** (`settings.rs:87-93`).

**Problem**: A single executor iteration that takes > 180 s on
OpenRouter (possible for large reasoning models on complex prompts) will
have its HTTP client abort mid-stream even though the *task* still has
420 s of budget left. The audit mentions task timeouts but does not
flag this mismatch between the HTTP-client timeout and the task-level
timeout. Neither client timeout is configurable from Settings.

**Recommendation**: make the SSE client timeout a function of
`task_timeout_secs` (e.g. `min(task_timeout_secs + 60, 900)`) or expose
explicit `openrouter_http_timeout_secs` / `ollama_http_timeout_secs`
settings. Document the relationship in `docs/EVALUATION.md`.

### 2.2 `check_planner` is a "key-exists" check, not a reachability probe

**Severity: Low (UX footgun)**

```rust
// ai.rs:589-592
pub async fn check_planner(state: tauri::State<'_, AppState>) -> Result<bool, String> {
    let key = state.settings.lock().unwrap().openrouter_api_key.clone();
    Ok(!key.is_empty())
}
```

**Problem**: The top-bar "planner ready / planner off" badge is based
solely on *key non-empty*, not on whether OpenRouter is actually
reachable and the key is valid. A revoked key, a typo'd key, or a
network outage all report "planner ready" until the user hits send and
gets a 401/connection error.

**Recommendation**: mirror `probe_ollama` for OpenRouter — a
`probe_openrouter` command that hits `https://openrouter.ai/api/v1/models`
with the configured key and reports reachability + auth status.

### 2.3 `check_executor` uses a 3 s probe — too aggressive during model warmup

**Severity: Low**

```rust
// ai.rs:598-606
let client = reqwest::Client::builder()
    .timeout(std::time::Duration::from_secs(3)) ...
```

**Problem**: A freshly-started `ollama serve` that is loading a 6.7B
model into VRAM can take > 3 s to respond to `/api/tags`. The UI will
flash "ollama offline" on startup even when Ollama is healthy and about
to be ready. Same applies after a GPU driver reset.

**Recommendation**: raise the health-probe timeout to 10 s, or do a
background retry on failure before marking the executor red.

### 2.4 `pending_confirms` uses `std::sync::Mutex` holding a `oneshot::Sender` across async boundaries

**Severity: Medium (subtle)**

```rust
// lib.rs:57-60
pub pending_confirms: Mutex<HashMap<String, oneshot::Sender<bool>>>,
```

**Problem**: The confirm-cmd flow inserts/removes oneshot senders under
a **sync** mutex. If any future refactor holds the lock across an
`.await`, the runtime will deadlock on the single-threaded pool. The
pattern also means a rapid double-click on "Approve" races: whichever
`take()` wins sends; the loser silently drops. Not wrong today — but
it is a hazard a future contributor will trip.

**Recommendation**: switch to `tokio::sync::Mutex` for collections that
are touched inside async command handlers, or use
`parking_lot::Mutex` and assert non-async scope via a type-level guard.

### 2.5 Goal planner's JSON extraction accepts trailing junk silently

**Severity: Low**

```rust
// controller.rs:695-715
fn parse_plan_json(s: &str) -> Option<Vec<PlanTask>> {
    let stripped = strip_code_fences(s);
    let start = stripped.find('{')?;
    let end = stripped.rfind('}')?;  // <-- uses last '}'
    let slice = &stripped[start..=end];
    ...
}
```

**Problem**: If the model returns *two* JSON objects (e.g. a thinking
blob followed by the real plan), the parser slices from the first `{`
to the *last* `}` — concatenating both into one invalid string that
`serde_json::from_str` rejects, and the whole call falls through to the
English-only heuristic fallback. This explains some "plan failed, using
heuristic" warnings on reasoning-tuned models.

**Recommendation**: find the first *balanced* JSON object via bracket
counting and parse that. Matches how the OpenAI / Anthropic SDKs handle
chain-of-thought responses.

### 2.6 Empty per-task history also discards the goal map

**Severity: Medium (exacerbates inter-task context loss)**

The main audit flags inter-task context loss (`controller.rs:454` passes
`Vec::<UiMessage>::new()`). But there's a second effect not called
out: each task re-loads `project_context_summary` from disk via
`ai::run_chat_turn`. If the executor created a new file in task 2
(say `src/jwt.ts`), the project map in memory on disk is stale until the
next `project_scan::scan_project` — which only runs at `start_goal`,
not between tasks. So task 3's "project_ctx" doesn't list the file the
previous task created; the executor has to rediscover it via `list_dir`.

**Recommendation**: Phase 1 should also add a lightweight project-map
delta update between tasks: diff `touched_files` into the persisted
`project_map` so subsequent tasks see newly-created files in their
grounding context.

### 2.7 Frontend "streaming synthesis" path can double-emit a bubble

**Severity: Low**

`Chat.tsx:105-122` has a fallback path: *"If streaming never produced
an executor bubble, synthesize one from the final response so the user
sees something."* Combined with the `streaming_role` matching in
the token handler (`Chat.tsx:44-62`), there's a race on slow machines:
the last tokens arrive between the `api.sendChat` resolution and
`setMessages` of the synthesized bubble, producing a duplicate.

**Recommendation**: make the synthesis path keyed on a stable
per-turn id (pass the turn id back from the backend in `ChatResponse`,
use it to dedupe), or remove the synthesis path once Phase 3's
deterministic bubble lifecycle is in place.

### 2.8 No accessibility affordances in the current UI

**Severity: Medium (blocks the "competitive with Devin/Windsurf" goal)**

- No ARIA roles on the 4-pane layout (`App.tsx:174+` uses plain `<div>`s).
- No keyboard navigation between panes.
- No focus-visible styling on buttons (`<button>Settings</button>` etc.).
- No reduced-motion query for the planned animations.

The UI/UX plan (Section 7 of the main audit) specifies colors, spacing,
and animation timings but does not mention a11y. Any claim of
"Devin-level UX" has to include WCAG-AA keyboard/contrast/ARIA at
minimum.

**Recommendation**: add an **a11y section** to `docs/UI_DESIGN.md` when
it's created in Phase 3/4. Pin targets: WCAG 2.1 AA, full keyboard
navigation, visible focus rings, `prefers-reduced-motion` respected in
the thinking-block collapse/expand.

### 2.9 `executor_iterations` is tracked but never surfaced

**Severity: Low (observability)**

`ai.rs:818` increments `executor_iterations` every loop pass, but the
counter is never returned in `ChatResponse` or emitted to the frontend.
This hides one of the most useful signals for diagnosing
"model stuck in a tool loop" — a user can see the UI chugging but has
no way to know the executor is on iteration 12 of 16.

**Recommendation**: include `executor_iterations` in the
`ChatResponse` payload and surface it as a subtle counter in the chat
header (e.g. `"executor · 7/16 steps"`). This also helps Phase 1's
circuit-breaker tuning.

---

## 3. Revised Issue Census (for triage)

Merging the main audit's list with the 9 additions above:

| Severity | Count (main audit) | Added by addendum | Total |
|----------|---------------------|-------------------|-------|
| Critical | 2                   | 0                 | 2     |
| High     | 4                   | 0                 | 4     |
| Medium   | 9                   | 4 (2.1, 2.4, 2.6, 2.8) | 13 |
| Low      | 7                   | 5 (2.2, 2.3, 2.5, 2.7, 2.9) | 12 |
| **Total** | **22**              | **9**             | **31** |

Phase 1 should pick up: 2.1 (client timeouts), 2.5 (balanced JSON
parser), 2.6 (project-map delta between tasks), 2.9 (surface iteration
count) alongside the items already listed in the main audit.

Phase 2 should pick up: 2.2 (probe_openrouter), 2.4 (async mutex for
pending_confirms).

Phase 3/4 should pick up: 2.7 (dedupe bubble synthesis) and 2.8 (a11y).

---

## 4. Unchanged Verdict

The main audit's bottom-line stands:

> **Not production-ready for general release. Usable for local-first,
> supervised, single-user operation with model and timeout tuning.**

This addendum does not move the composite 5.6/10 score; it sharpens
the Phase 1 / Phase 2 work items and flags accessibility as a
first-class concern for the UI phases (which the main audit did not).

Estimated effort unchanged: **5–8 weeks of focused work to a
shippable v1.0**.

---

## 5. What I Did Not Do This Pass (deliberately)

- **No code changes.** This is analysis only, per the request.
- **No PR.** No branch was created.
- **No re-running of `bun run typecheck` / `bun run build`** — the
  desktop app uses Cargo for the backend; the top-level `bun`
  commands in `AGENTS.md` / `CLAUDE.md` apply to the research-snapshot
  CLI under `src/`, not `desktop/`. This is itself a documentation
  inconsistency worth flagging: `docs/EVALUATION.md` and the root
  guidance files conflate the two subtrees. The Section-5 doc
  restructuring proposal in the main audit would fix this.

If you want me to **execute Phase 1** (the core stability fixes from
Section 8 of the main audit + items 2.1, 2.5, 2.6, 2.9 from this
addendum), say so and I'll branch from `main`, implement, and open a
PR.

---

*Addendum end. All claims grounded in actual code at `main` @ session time.*
*Companion to `FULL_SYSTEM_AUDIT.md` — do not read in isolation.*
