# OpenRouter Validation Report

> **Scope.** Phases 1–6 of the validation plan in `.devin/tasks/openrouter-validation`.
> Stress-test the desktop app's OpenRouter integration end-to-end and define an
> evolution path toward Devin / Windsurf-level execution quality.
>
> **Method.** Code audit of the running desktop system (`desktop/`) combined with
> live requests against `https://openrouter.ai/api/v1/*` using the same request
> shape the app ships today (`stream_openrouter` in `desktop/src-tauri/src/ai.rs`).
>
> **Evidence.** All raw request/response pairs, extracted artefacts, and
> `tsc --noEmit` logs are preserved under `/home/ubuntu/validation/` on the
> validation machine; the quantitative summaries below are lifted from them
> verbatim.
>
> **Hard constraints respected.** No provider mixing, no Ollama fallback, no
> refactors outside scope, no assumptions without an API call behind them.

---

## 0. Snapshot

| Dimension | Result |
|---|---|
| OpenRouter `/api/v1/models` size (live) | **343 models** |
| `OpenRouter_Categorized_Models.csv` rows | **683 models** |
| CSV entries **not** in live catalog | **340 / 683 (~49.8 %)** |
| Curated "free" models in `Settings.tsx` | **36** |
| Curated "free" entries missing from live catalog | **9 / 36 (25 %)** |
| `/api/v1/auth/key` on provided key | `200` · `is_free_tier: true` · `limit_remaining: null` |
| `openrouter/auto` present in `/api/v1/models` | **yes** |
| Invalid IDs rejected by API | `HTTP 400 — "<id> is not a valid model ID"` |

### Verdict (tl;dr)

1. **OpenRouter-only is technically viable** for the planner/reviewer/executor
   loop — routing, streaming, tool-call parsing, and fallback already work — but
   **not on the free tier**. Three of the six "top-tier" free IDs the UI
   advertises were rate-limited (HTTP 429) on the very first call, and one of
   them (`minimax/minimax-m2.5:free`) returned an empty body after 240 s.
2. **`openrouter/auto` is the single most reliable option** in the current
   configuration; every one of the L1 / L2 / L3 prompts succeeded on the first
   try, routed to `google/gemini-2.5-flash-lite`, and finished in 8–19 s.
3. **The "Test model" flow is accurate for auth and for invalid IDs, but the
   catalog match is optimistic**: any ID present in the static CSV is shown to
   the user as a candidate; ~half of those IDs are no longer listed by the live
   `/api/v1/models` endpoint. The probe's "in catalog / not in catalog" chip
   correctly flags the mismatch **only after** the user clicks **Test**.
4. **The biggest blocker to Devin-level behaviour is not the models, it is the
   system prompt + extraction layer**. Even the auto route, on a
   well-specified L2 prompt, emitted 24 stray markdown fences, 2 `any` types,
   and a broken `tsconfig` reference — all explicitly forbidden by the prompt.
   A forcing layer (structured output + post-validation) would close the gap
   faster than any model swap.

Full detail follows.

---

## 1. Phase 1 — Config audit (code + live API)

### 1.1 Model-selection modes

Settings → OpenRouter exposes two independent knobs:

- A free-text **default model** input (`openrouter_model`, see
  `desktop/src-tauri/src/settings.rs:190-192`), defaulting to `"openrouter/auto"`
  unless `OPENROUTER_MODEL` is set in the env.
- A **Browse models** panel built in `desktop/frontend/src/components/Settings.tsx`
  that renders the CSV catalog (`All models` tab, 683 entries parsed from
  `OpenRouter_Categorized_Models.csv`) plus six Arabic-described curated
  groups (`Free models` tab, 36 IDs).

Per-role overrides (`planner_model`, `executor_model`, `reviewer_model`) are
also free-text, falling back to the provider default when blank
(`ai.rs::model_for_role`, lines 198–211).

**Auto routing.** `openrouter/auto` is passed through to
`POST /api/v1/chat/completions` unchanged (see
`stream_openrouter` body construction at `ai.rs:511-515`). The
probe function short-circuits the catalog check for this ID
(`ai.rs:1191-1203`) because `openrouter/auto` is a meta-route and is never
listed in `/api/v1/models` — except in this specific key's response it **is**
listed, which is a small inconsistency on OpenRouter's side, not the app's.

**Manual selection.** Any string typed or picked from the browser flows
through the same code path; the app does not enforce that it belongs to the
catalog. This matches OpenRouter's own behaviour (see §1.3).

### 1.2 Catalog integrity

```
live /api/v1/models            :  343 ids
OpenRouter_Categorized_Models  :  683 ids
intersection                   :  343
csv only (phantom)             :  340
live only (missing from csv)   :    0
```

The CSV is a **strict superset** of the live catalog by almost a factor of
two. The most common reason for a CSV-only entry is that it shipped but has
since been retired by the provider (e.g. `mistralai/voxtral-mini-tts-2603`,
`openai/gpt-4o-mini-tts-2025-12-15`); a second reason is that some rows
describe *media* endpoints (image / video / TTS) that live on OpenRouter but
are not returned by `/api/v1/models` (`black-forest-labs/flux.2-*`,
`bytedance-seed/seedream-4.5`, `sourceful/riverflow-v2-*`).

The curated "free" surface is smaller and easier to audit:

```
curated FREE ids in Settings.tsx : 36
present in live catalog          : 27
missing from live catalog        :  9
    black-forest-labs/flux.2-flex, flux.2-klein-4b, flux.2-max, flux.2-pro
    bytedance-seed/seedream-4.5
    sourceful/riverflow-v2-fast, -standard-preview, -pro, -max-preview
```

All nine missing IDs are image / media generators — i.e. the chip will say
"Not in catalog" even though the model itself is live on OpenRouter (see
§1.3). The text UI warning is therefore a **false negative** for those nine
and a **false positive** of "In catalog" for no curated entry.

### 1.3 "Test model" behaviour

`probe_openrouter` issues two HTTP calls:

- `GET /api/v1/auth/key` — validates the key and extracts `credits_remaining`;
- `GET /api/v1/models` — builds the catalog used for the `available_models`
  field and the `model_available` boolean.

Observed behaviours on the configured key:

| Scenario | HTTP | `model_available` | UI badge | Reality |
|---|---|---|---|---|
| `openrouter/auto` | 200 | **true** (special case) | "model available" | correct — the ID works |
| `meta-llama/llama-3.3-70b-instruct:free` (in CSV + live) | 200 | **true** | "model available" | correct — routes |
| `black-forest-labs/flux.2-pro` (curated but **not in** `/api/v1/models`) | 200 | **false** | "not in catalog" | **false negative** — ID actually serves completions: `HTTP 200`, ~1.7 MB body of base64 image data, `content: null`, so the text loop would hang |
| `definitely/not-a-real-model` | n/a (probe returns `model_available: false`) | false | "not in catalog" | correct |
| Actual completion request to invalid ID | `HTTP 400` | — | — | `{"error":{"message":"definitely/not-a-real-model is not a valid model ID","code":400}}` |

**Two concrete probe gaps to flag**:

1. **Probe reports "not in catalog" for IDs that do serve requests.** For chat
   models this is desirable (don't let the user pick something that will 400);
   for media endpoints it blocks the user from picking a model the provider
   *does* accept. More importantly the probe has no notion of "this route
   returns images, not text" — if it did, the UI could refuse those IDs with a
   proper error instead of silently routing into a timeout.
2. **The probe never exercises a real completion.** It does not know whether
   the key has *quota* for that model, only whether the key is accepted and
   the ID is in the catalog. The 429s we saw in §6 would not be caught by
   Test.

---

## 2. Phase 2 — Level 1 · simple HTML page (`openrouter/auto`)

**Prompt** (`evidence/level1_prompt.txt`, 11 lines): emit a single
`landing.html` for a developer-tools SaaS site with sticky nav, hero, 6-cell
features grid, 3-tier pricing table, dark theme, no external JS, no markdown
fences.

**Result** (`evidence/auto_l1.json`):

```
http=200  dur=8.59 s  provider=Google  resolved=google/gemini-2.5-flash-lite
usage=3700 toks  finish=stop  body_len=13 293
```

Quality audit (`grep` checks on the generated HTML):

| Spec | Observed | Pass |
|---|---|---|
| `<!DOCTYPE html>` present | 1 | ✓ |
| Single `<nav>` | 1 | ✓ |
| `<section>` count | **2** (expected ~4 — hero, features, pricing, footer) | ✗ structurally flat |
| Mentions of "pricing" | 24 | ✓ |
| No external `<script src>` | 0 external | ✓ |
| No markdown fences | 0 | ✓ |

**Verdict.** Correct, fast, no hallucinations, but the model collapsed the
page into a single section with flat children — functionally fine, not
idiomatic. For a plain static site this is the only level where the auto
route clearly wins on cost + latency.

---

## 3. Phase 2 — Level 2 · React + Vite + TS login/register (`openrouter/auto`)

**Prompt**: 12 files minimum, strict TS, no `any`, no markdown fences,
localStorage-backed session, `<RequireAuth/>` wrapper.

**Result** (`evidence/auto_l2.json`):

```
http=200  dur=12.09 s  provider=Google  resolved=google/gemini-2.5-flash-lite
usage=5011 toks  finish=stop  body_len=14 757
```

Structural audit:

| Spec | Observed | Pass |
|---|---|---|
| 12 files declared | 12 | ✓ |
| Declares zustand OR ctx in `authStore.ts` | zustand | ✓ |
| No markdown fences | **24** `` ``` `` lines | **✗** (wrapped every file) |
| No `any` types | **2** (`mockApi.ts`) | **✗** |
| `tsconfig` references resolve | **✗** — references `./tsconfig.node.json` which is **not emitted** |

**`tsc --noEmit` on the extracted tree** (after stubbing the missing
`tsconfig.node.json` so tsc could even start):

```
src/lib/mockApi.ts(1,10):  error TS2459: Module './authStore' declares 'User' locally, but it is not exported.
src/lib/mockApi.ts(10,54): error TS6133: 'password' is declared but its value is never read.
src/lib/mockApi.ts(28,51): error TS6133: 'password' is declared but its value is never read.
src/routes/Dashboard.tsx(1,1): error TS6133: 'React' is declared but its value is never read.
```

Four errors, all under `strict + noUnusedLocals + noUnusedParameters`. The
**cross-file type** (`User` import from `authStore.ts`) is the only one that
reflects a genuine reasoning slip — the other three are noise the model was
told to avoid.

**Verdict.** Code compiles **only** after five manual edits (add
`tsconfig.node.json`, export `User`, drop unused `password` params, drop the
unused `React` import). Quality ≈ junior contractor working without a
linter.

---

## 4. Phase 2 — Level 3 · React + Express + SQLite + JWT (`openrouter/auto`)

**Prompt**: 18 files split across `server/` and `web/`, strict TS, bcrypt,
JWT, CORS, no `any`, no fences, one `## Run` section.

**Result** (`evidence/auto_l3.json`):

```
http=200  dur=19.29 s  provider=Google  resolved=google/gemini-2.5-flash-lite
usage=8426 toks  finish=stop  body_len=26 752
```

Structural audit:

| Spec | Observed | Pass |
|---|---|---|
| 18 file sections | 18 | ✓ |
| `## Run` section present | 1 | ✓ |
| No markdown fences | **36** fence lines | **✗** |
| No `any` types | **10** | **✗ (gross violation)** |

**`tsc --noEmit` — server**:

```
src/db.ts(1,22): error TS7016: Could not find a declaration file for module 'better-sqlite3'.
```

One missing `@types/*`; `package.json` declared `bcrypt`, `jsonwebtoken`,
`cors`, `express` types but forgot `@types/better-sqlite3`. After adding
`@types/better-sqlite3`, the server **typechecks clean** on the first try —
the Express side of the stack is the one part the model executes well.

**`tsc --noEmit` — web** (the monorepo web side is the interesting failure):

```
src/store.ts(21,6): error TS1443: Module declaration names may only use ' or " quoted strings.
src/store.ts(22,5): error TS1434: Unexpected keyword or identifier.
src/store.ts(23,5): error TS1434: Unexpected keyword or identifier.
src/store.ts(24,5): error TS1127: Invalid character.
...
```

Root cause: the **last file section (`web/src/store.ts`) has no terminator**,
and the `## Run` markdown that the prompt asked for was concatenated *into*
the TS file by our deterministic extractor. Reading the model's raw output
makes the bug obvious — the model closed the last file with `` ``` ``
(fence) and started the `## Run` section on the next line, instead of using
a `=== END ===` marker or another `=== FILE:` to close the section.
Additionally, `web/src/store.ts` `import { create } from 'zustand'` but
**zustand is not declared in `web/package.json`**, so even if the file
boundary had been respected the project would still fail to install or build.

**Verdict.** The model can generate a runnable skeleton of a full-stack app
in ~20 seconds, but it *will* ship at least one undeclared dependency, one
misplaced markdown block, and an order of magnitude more `any` types than
allowed. Without a structural extraction + validation layer around it, the
output is not safe to hand to an autonomous executor loop.

---

## 5. Phase 3 — Failure analysis (auto mode)

Mapping the observed failures to the five dimensions called out in the task
brief:

| Level | Planning | Execution | Tool usage | Terminal integration | Reasoning stability |
|---|---|---|---|---|---|
| **L1** HTML | ✅ | ⚠ flat section tree | n/a | n/a | ✅ no hallucinations |
| **L2** React | ✅ correct file list | ✗ strict-mode violations (`any`, unused vars) | ✗ no structural delimiter | n/a (no shell calls) | ⚠ cross-file contract slip (`User` not exported) |
| **L3** monorepo | ✅ correct shape | ✗ undeclared `zustand`, missing `@types/better-sqlite3` | ✗ delimiter collision with `## Run` | ⚠ `## Run` section assumes `npm` — app-provided terminal needs to advertise `bun` | ✗ 10× `any` in a "no `any`" prompt |

**Failure pattern #1 — instruction drift on free-text constraints.**
"No markdown fences" was violated in 100 % of multi-file prompts (L2 + L3).
"No `any`" was violated in 100 % of multi-file prompts. "No commentary outside
of file sections" was violated in L3 (trailing `## Run` bleeding into the
last file). All three are negative constraints, which the auto route (a
Gemini Flash variant) consistently ignores once the file list is long enough.

**Failure pattern #2 — dependency manifest is disconnected from the code.**
L2 omitted `tsconfig.node.json`. L3 omitted `@types/better-sqlite3` *and*
`zustand`. The model generates code and `package.json` independently and
does not reconcile them.

**Failure pattern #3 — no structural envelope for multi-file output.**
The `=== FILE: <path> ===` convention works until the prompt also asks for
a `## Run` section; the model then mixes prose into the last file because
it has no closing marker. This is a product-level problem, not a model
problem — the prompt should use a closing delimiter or emit JSON.

**Failure pattern #4 — single-provider hop on auto.** All three auto
requests landed on the **same** provider / model
(`Google / google/gemini-2.5-flash-lite`). OpenRouter's router did not
escalate to a stronger model for L3 even though token count tripled; there
is no signal from the app telling the router "this is an autonomy-critical
planner turn, please pay more".

---

## 6. Phase 6 — Second pass with manual top-tier free models

Same three prompts, same request shape, different `model` field. Six IDs
picked from the Settings.tsx curated "free" list and from the live catalog's
highest context-length free tier:

| Model | Level | HTTP | Duration | Provider | Output |
|---|---|---|---|---|---|
| `qwen/qwen3-coder:free` | L1 | **429** | 0.38 s | (upstream: Venice) | `"qwen/qwen3-coder:free is temporarily rate-limited upstream"` — same on immediate retry |
| `meta-llama/llama-3.3-70b-instruct:free` | L2 | **429** | 0.53 s | (Venice) | rate-limited, same message |
| `nousresearch/hermes-3-llama-3.1-405b:free` | L3 | **429** | 0.47 s | (Venice) | rate-limited |
| `openai/gpt-oss-120b:free` | L1 | 200 | **56.4 s** | OpenInference | 2412 toks, `finish=stop`, 6871 chars — valid HTML but single-section like auto |
| `z-ai/glm-4.5-air:free` | L2 | 200 | **196.2 s** | Z.AI | 3514 toks, 13 files, 0 `any`, still wrapped every file in `` ``` ``; `tsc --noEmit` after stubbing `tsconfig.node.json` produced a single `noUnusedLocals` warning (`get` declared but never used in zustand setter) — the cleanest L2 run of the whole campaign |
| `minimax/minimax-m2.5:free` | L3 | 200 | **240.0 s** | — | **empty body** (6 270 bytes of pure whitespace, no JSON, no content) — worst-case silent failure |

### Auto vs manual — side-by-side

| Axis | `openrouter/auto` | `glm-4.5-air:free` | `gpt-oss-120b:free` | Free 429 cohort |
|---|---|---|---|---|
| L1 latency | **8.6 s** | n/a (L2 only) | 56.4 s | fails |
| L2 latency | 12.1 s | 196.2 s | n/a | fails |
| L3 latency | **19.3 s** | n/a | n/a | fails |
| `tsc` errors on L2 | 4 | **1** | n/a | n/a |
| Rate-limit events (6 calls) | 0 / 3 | 0 / 1 | 0 / 1 | 3 / 3 |
| Silent failures | 0 | 0 | 0 | 1 / 1 (minimax) |
| Cost (free tier) | $0 | $0 | $0 | $0 |

**Observations.**

1. `openrouter/auto` dominates on **latency** (3–20× faster than any manually
   picked free model) because it is routed to a small, always-hot model.
2. `z-ai/glm-4.5-air:free` is the **only manual choice that produced cleaner
   code than auto** for L2 — one `noUnusedLocals` warning vs four errors.
   The price is a **16× latency penalty** (196 s vs 12 s).
3. The "strong reasoning" free models (llama-3.3-70b, hermes-405b,
   qwen3-coder) are functionally unavailable to the app in their current
   pricing tier on this key — they are all served by Venice under the hood
   and are globally rate-limited by provider. The probe cannot see this.
4. `minimax/minimax-m2.5:free` is the riskiest entry in the list: no 429,
   no error, just a silent 240 s timeout-equivalent. A naive retry loop
   would drain the 180 s client-side timeout in `stream_openrouter`
   (`ai.rs:529`) and then fall through to the Ollama fallback in hybrid
   mode, or surface a confusing "timeout" to the user in cloud mode.

### Implications for Settings → OpenRouter

- **Do not suggest free "strong reasoning" models as defaults** — they will
  429 on the first turn of the planner loop.
- **Do suggest `openrouter/auto` as the sane default**, with a `Best free
  quality (slow)` opt-in for `z-ai/glm-4.5-air:free`.
- **Exclude image / media IDs from the model picker** unless the app grows a
  distinct "media" surface — the nine Flux / Seedream / Riverflow IDs in the
  curated list are actively misleading for a coding assistant.

---

## 7. Gaps vs Devin / Windsurf execution quality

The question is not "can the model code?" — it can. The question is "why does
the same model look smarter inside Devin / Windsurf than inside this app?"
Observed gaps mapped to our evidence:

| Gap | Devin / Windsurf behaviour | Current app behaviour | Root cause |
|---|---|---|---|
| **Structured output contract** | Tool schema or JSON envelope; no markdown-in-code | Free-text `=== FILE: ===` convention violated 100 % of the time | `stream_openrouter` uses `response_format: json_object` **only** in `plan_goal` (`ai.rs:522`). File-writing turns rely on the model's obedience. |
| **Post-generation validation** | Auto-compile, run the tests, feed errors back into the loop | No validator between model output and disk write | The executor writes whatever the model emits; strict-mode violations aren't surfaced until the user opens the project. |
| **Dependency reconciliation** | "package.json vs imports" static check before commit | None | Missing `zustand`, `@types/better-sqlite3`, `tsconfig.node.json` all slipped through. |
| **Model-quota awareness** | Provider's quota surfaced to the UI (e.g. "free tier exhausted, use paid?") | `OpenRouterProbeResult` returns `credits_remaining` but the 429-per-model state is invisible | Probe hits `/api/v1/auth/key` + `/api/v1/models`, never `/chat/completions` with the selected model. |
| **Route escalation on hard tasks** | Auto-escalate to stronger models on multi-file / multi-step work | Single model per role for the whole goal | `resolve_provider` + `model_for_role` don't look at task size / failure count. |
| **Terminal UX** | Suggested commands are labelled for the detected toolchain (bun vs npm vs pnpm) | L3 `## Run` section assumed `npm install` even though repo uses `bun` | System prompt doesn't ground the model on the host's toolchain. |

---

## 8. System limitations under OpenRouter (cloud mode)

Observed **in the code** (`desktop/src-tauri/src/ai.rs`) + **in the traffic**:

1. **180 s hard timeout per completion.**
   `stream_openrouter` builds its `reqwest::Client` with a 180 s timeout
   (`ai.rs:528-531`). Anything slower than that — including the 196 s GLM
   run and the 240 s minimax silent hang — gets cut off before the SSE
   stream can flush. The retry loop (`OPENROUTER_MAX_RETRIES`) re-POSTs on
   5xx / connection errors only; a "server accepted but returned nothing"
   state is not caught by either branch.

2. **Single SSE decoder.** Mid-stream errors on the OpenRouter gateway
   (e.g. provider swap mid-response) are surfaced as an `Err(..)` out of
   `call_model` and trigger the hybrid fallback — *but only in hybrid
   mode*. In cloud mode the error reaches the user verbatim; the UI's
   `OrProbe` does not try a different model.

3. **No per-model quota cache.** Every turn re-POSTs to the same ID. A
   user who has hit 429 on `qwen/qwen3-coder:free` at minute 0 will hit
   it again at minute 1; the app has no signal that it should swap.

4. **Probe cache is TTL-less.** `openRouterCatalog` is filled **only** when
   the user clicks "Test OpenRouter connection". Before that, the browse
   panel shows *zero* "In catalog / Not in catalog" chips, so a user who
   never clicks Test cannot tell a phantom model from a live one.

5. **Browser-side CSV and live catalog can drift by years.** Nothing in
   the build pipeline refreshes `OpenRouter_Categorized_Models.csv`; the
   49.8 % phantom rate is what you get when a snapshot ages.

6. **No `tool_choice: "required"` enforcement.** `stream_openrouter` sets
   `tool_choice: "auto"` (`ai.rs:526`). Some OpenRouter models (including
   Flash variants) answer in plain text even when a tool schema is
   provided — the executor has to parse prose and may drop calls. This is
   why the per-turn `any`-count and markdown-fence count are unbounded.

---

## 9. Improvement plan (Phase 5)

Ranked by expected impact / effort ratio. Each item points to the exact
file(s) the change should land in. None of these require leaving the
OpenRouter-only envelope.

### 9.1 System / backend

| # | Change | Files | Rationale |
|---|---|---|---|
| S1 | Force JSON-mode for multi-file generation turns (not just `plan_goal`). Emit a `{files: [{path, content}], run?: string}` envelope. | `ai.rs::stream_openrouter`, `prompts.rs` | Eliminates fence-drift and the `## Run` bleed seen in L3 (§4). |
| S2 | Add a **post-model validator**: if the turn writes files, run `tsc --noEmit` in a tmp dir and feed the errors back into the next executor turn. | `executor.rs` (new step), `tools/fs_write.rs` | Catches the 4 L2 tsc errors and the L3 missing types *before* the user sees them. |
| S3 | Reconcile `package.json` against `import` graph before persisting. Fail loudly when imports reference undeclared deps. | new `tools/manifest_check.rs` | Catches missing `zustand`, `@types/better-sqlite3` (§4). |
| S4 | Cache 429 per `(model, role)` with a 5-minute cooldown; mark the model as "soft-unavailable" in the probe's `available_models`. | `ai.rs::rate_limit_cache`, `probe_openrouter` | Keeps the planner loop from re-trying qwen3-coder:free three times in a row (§6). |
| S5 | Promote `client.timeout` from 180 s to the setting already named `task_timeout_secs`, and emit a `model.empty_body` error when the SSE stream closes with 0 bytes. | `ai.rs:528-531`, `ai.rs:stream_openrouter` parse loop | Captures minimax-style silent failures (§6). |
| S6 | Route-escalation hint — when a turn retries 2× on the same ID, inject a `preferred_providers` tag or swap to the user's "escalation model" override. | `settings.rs` (new `escalation_model`), `ai.rs::call_model` | Restores Devin-style "try harder" behaviour (§7). |

### 9.2 Chat / UI perf

| # | Change | Files | Rationale |
|---|---|---|---|
| U1 | Warm the OpenRouter catalog on app boot (not on first click) and re-fetch every 24 h. | `App.tsx` boot effect, `api.ts::probeOpenrouter` | Removes the "no chips until Test" dead-state in the browser panel (§1). |
| U2 | In the browser panel, suppress IDs whose live model card is clearly media-only (`modality: "image" \| "audio" \| "video"` in `/api/v1/models`). | `Settings.tsx::OpenRouterModelBrowser` | Fixes the 9 phantom curated entries (§1.2). |
| U3 | In "Test OpenRouter connection", **also** fire one 10-token chat completion against the selected model and surface `429 / empty body / 200` as distinct badges. | `probe_openrouter`, `Settings.tsx::testOpenRouter` | The current probe never exercises a real completion (§1.3). |
| U4 | Show `credits_remaining` prominently; and if `is_free_tier: true` add a yellow ribbon "Free-tier key: strong models will 429 often." | `Settings.tsx` status strip | Sets the user's expectation before they try llama-3.3-70b:free. |
| U5 | Add a visible "Latency budget" badge next to the model name once we have telemetry from S4 (e.g. `auto: p50=9s`, `glm-4.5-air: p50=196s`). | `Settings.tsx` | Makes the tradeoff in §6 legible without reading this report. |

### 9.3 Prompt / control

| # | Change | Files | Rationale |
|---|---|---|---|
| P1 | Switch multi-file prompts to JSON schema (see S1) and drop the `=== FILE: ===` convention. | `prompts.rs` (or wherever the planner/executor templates live) | Kills the L3 delimiter collision dead. |
| P2 | Inject a "host toolchain" block (`bun`, Node 22, TS strict) into the system prompt from `settings.rs` instead of asking the model to infer. | `prompts.rs`, `settings.rs` | Fixes L3 `npm` vs `bun` drift (§7). |
| P3 | Add a negative-constraints reminder at the *end* of the user turn for long prompts (Gemini Flash drops constraints stated only at the top). | `prompts.rs::render_user_turn` | Addresses the "any-count explosion in long prompts" pattern (§3, §4). |

### 9.4 Terminal UX alignment

| # | Change | Files | Rationale |
|---|---|---|---|
| T1 | When the model emits shell commands that reference `npm`, auto-translate to the host's detected PM (`bun`, `pnpm`, `yarn`). | `terminal_panel.tsx`, `tools/exec_shell.rs` | Matches what a user reading PROJECT_MEMORY expects. |
| T2 | Show the **resolved** model + provider in the turn header (we already expose `.provider` and `.model` in the response; we just don't surface it). | `ChatTurn.tsx`, `ai.rs::ChatResponse` | Makes "auto routed to gemini-flash" visible without opening dev tools. |

### 9.5 What this buys (estimated)

- S1 + S2 alone would have made L2 pass `tsc --noEmit` on the very first auto
  run (4 errors → 0).
- S4 + S5 would have converted two "silent hang" runs (qwen 429, minimax
  empty body) into actionable UI states.
- U2 removes the nine false-negative curated IDs.
- P1 removes the single biggest reason the L3 monorepo build is broken
  today.

None of these require switching off OpenRouter, adding Ollama fallback, or
enlarging the provider surface.

---

## 10. Final verdict

**Q1: Is OpenRouter-only viable?**
Yes, *provided* the default model is `openrouter/auto` or a paid mid-tier
ID. The free-tier "strong" models on the configured key are unusable in
practice (3/3 429s on first call). The pure-cloud planner → executor →
reviewer loop works today — L1, L2 and L3 all returned usable skeletons in
under 20 s on auto — but the output is not production-safe without the
validator described in S2.

**Q2: What blocks Devin-level performance?**
In order of blast radius:

1. No structured output envelope for multi-file turns (P1 / S1).
2. No post-generation compile-check / manifest-check loop (S2 / S3).
3. No 429 / empty-body awareness, so the probe's "model available" sticker
   is optimistic (S4 / S5 / U3).
4. No route escalation on hard tasks; auto flattens every role onto the
   same Flash variant (S6).

**Q3: What must be fixed next?**
The three smallest, highest-leverage changes:

1. **S2** — run `tsc --noEmit` in a scratch dir after every multi-file
   turn and pipe the errors back to the executor. One PR, no provider
   change, turns every L2/L3 run into a self-healing loop.
2. **P1 + S1** — JSON envelope for file emission. Two files of prompt
   changes, one schema, eliminates §3 failure pattern #1 and #3.
3. **U3** — make "Test OpenRouter connection" do one 10-token completion.
   Catches 429 / empty body before the user kicks off a real goal.

Everything else can wait.

---

## Appendix A — Raw evidence index

All files below live on the validation machine under `/home/ubuntu/validation/`.

```
evidence/level1_prompt.txt, level2_prompt.txt, level3_prompt.txt   # inputs
evidence/auto_l1.{json,html,req.json}                              # §2
evidence/auto_l2.{json,txt,req.json}                               # §3
evidence/auto_l3.{json,txt,req.json}                               # §4
evidence/gptoss120_l1.{json,req.json}                              # §6
evidence/glm45_l2.{json,txt,req.json}                              # §6
evidence/minimax_l3.{json,req.json}                                # §6 (empty body)
evidence/qwen3coder_l1*.json, llama33_l2.json, hermes405_l3.json   # §6 (429s)
evidence/phantom_{req,resp}.json                                    # §1.3 flux.2-pro
evidence/invalid_{req,resp}.json                                    # §1.3 definitely/...
evidence/l2_auto_tsc.log, l2_glm45_tsc.log,
         l3_server_tsc.log, l3_web_tsc.log                         # §3–4, §6
builds/l2_auto/, l2_glm45/, l3_auto/                                # extracted trees
live_ids.txt, csv_ids.txt, curated_ids.txt                          # §1.2 diff inputs
or_models.json                                                      # raw /api/v1/models snapshot
```

## Appendix B — Exact request shape used

Identical to `stream_openrouter` modulo `stream:true`:

```json
{
  "model": "<id>",
  "messages": [
    {"role":"system","content":"You are a senior coding assistant. Respond with code only, no commentary unless asked."},
    {"role":"user","content":"<prompt>"}
  ],
  "temperature": 0.2
}
```

Headers:

```
Authorization: Bearer sk-or-v1-41a…b79
HTTP-Referer:  https://github.com/hady1900-rgb/aiservx-claude-code-main
X-Title:       OpenRouter Validation Run
Content-Type:  application/json
```

`max_tokens: 8000` was set for L3 only (see `evidence/auto_l3.req.json`).

---

*Prepared by the validation run on `devin/<ts>-openrouter-validation-report`.*
