# خطة التطوير — Development Plan

**تاريخ الإنشاء**: بعد تنفيذ المرحلة 1 (commit `7ca666e`)
**آخر تحديث**: بعد تنفيذ المرحلة 2 (Phase 2 — Provider Routing)
**المرجع**: `FULL_SYSTEM_AUDIT.md` + `FULL_SYSTEM_AUDIT_ADDENDUM.md` + `docs/PROVIDER_ROUTING.md`
**التقييم الحالي**: 7.0 / 10 (كان 6.4 قبل المرحلة 2)

---

## 0. الملخص التنفيذي

النظام هو **تطبيق Tauri desktop** (Rust backend + React frontend) يوفر
مساعد برمجة تلقائي متعدد الوكلاء (planner → executor → reviewer).

**ما تم إنجازه (المرحلة 1)**:
- الـ Reviewer يرى نتائج الأدوات الفعلية (كان يرى نص فقط) → يمنع مرور executor مهلوس
- Fallback حقيقي Ollama → OpenRouter (كان بدون fallback)
- نافذة منزلقة للسياق (20 رسالة) → يمنع تجاوز context window
- JSON retry + balanced bracket parsing → يقلل فشل التخطيط
- ملخص المهام السابقة بين المهام → يحل فقدان السياق
- `Mutex` → `RwLock` → قراءات متزامنة بدون تنافس

**ما تم إنجازه (المرحلة 2)**:
- `ProviderMode` صريح (`Cloud` / `Local` / `Hybrid`) + حقول per-role (`planner_model`, `executor_model`, `reviewer_model`)
- `call_model` dispatcher موحَّد: resolve → primary → fallback-on-error → `ai:error` metadata
- استبدال كل `stream_*` المباشرة (planner / executor / reviewer) بـ `call_model`
- `probe_openrouter` (reachable + key valid + model available + credits) — مرآة لـ `probe_ollama`
- Health probe timeouts 3s → 10s (cold-start + remote LAN)
- `provider` + `model` metadata على كل حدث `ai:step`
- `pending_confirms` من `std::sync::Mutex` إلى `tokio::sync::Mutex` (Addendum 2.4)
- `Settings` UI: Provider Mode selector + per-role model fields + زر Test OpenRouter
- توثيق `docs/PROVIDER_ROUTING.md` (وضعي الفشل، cost/perf، extensions)

**العوائق المتبقية** (مرتبة بالأولوية):
1. **واجهة المستخدم مستوى developer** — لا thinking blocks، لا تفرقة بصرية (Phase 3)
2. **لا يوجد `search_file`/`grep` tool** — التنقل في المشاريع الكبيرة بطيء
3. **لا يوجد `response_format: json_object`** — الموديلات الصغيرة تلف JSON بـ prose

**الحكم**: ليس جاهزاً للإطلاق العام. صالح للاستخدام المحلي المراقب.
الوصول لـ v1.0 يتطلب 5-6 أسابيع عمل مركّز.

---

## ✅ المرحلة 1 — استقرار أساسي (مُكتملة)

| # | الإصلاح | الملف | الحالة |
|---|---------|-------|--------|
| 1 | نافذة منزلقة للسياق (آخر 20 رسالة user/assistant) | `ai.rs:259-306` | ✅ |
| 2 | Reviewer يرى نتائج الأدوات الفعلية (✓/✗ + output) | `ai.rs:1135-1178` | ✅ |
| 3 | Fallback حقيقي في `call_executor_with_fallback` (Ollama → OpenRouter) | `ai.rs:591-626` | ✅ |
| 4 | Bracket counting في `parse_plan_json` بدل `rfind('}')` | `controller.rs:758-813` | ✅ |
| 5 | JSON retry في `plan_goal` عند فشل الـ parse | `controller.rs:721-755` | ✅ |
| 6 | ملخص المهام السابقة يُحقن في سياق المهمة التالية (500 حرف) | `controller.rs:630-686` | ✅ |
| 7 | `Mutex` → `RwLock` للـ settings (قراءات متزامنة بدون تنافس) | `lib.rs` + كل الاستدعاءات | ✅ |
| 8 | 5 اختبارات جديدة (balanced JSON, nested braces, edge cases) | `controller.rs:1012-1054` | ✅ |

**المتبقي من المرحلة 1** (عنصر واحد):

| # | المهمة | الملف | التفاصيل |
|---|--------|-------|----------|
| 1 | إضافة `response_format: { type: "json_object" }` لاستدعاء goal planner | `ai.rs` body في `stream_openrouter` / `stream_ollama` | يقلل prose/markdown wrapping من الموديلات الصغيرة |

---

## ✅ المرحلة 2 — نظام توجيه المزودين (مُكتملة)

**الهدف**: تحويل التوجيه الضمني (key present → cloud, else local) إلى نظام صريح قابل للإعداد مع fallback.

**المرجع**: `FULL_SYSTEM_AUDIT.md` Section 6 + Addendum 2.2 + `docs/PROVIDER_ROUTING.md`

| # | المهمة | الملف | الحالة |
|---|--------|-------|--------|
| 1 | إضافة `ProviderMode` enum (`Cloud` / `Local` / `Hybrid`) | `settings.rs` | ✅ |
| 2 | إضافة حقول `planner_model`, `reviewer_model`, `executor_model` | `settings.rs` | ✅ |
| 3 | بناء `call_model` dispatch + `resolve_provider` routing | `ai.rs` | ✅ |
| 4 | استبدال كل استدعاءات `stream_ollama`/`stream_openrouter` المباشرة بـ `call_model` | `ai.rs` | ✅ |
| 5 | إضافة `probe_openrouter` (فحص وصول + صلاحية المفتاح + credits) | `ai.rs` + `lib.rs` | ✅ |
| 6 | رفع timeout الـ health probe من 3s إلى 10s (Addendum 2.3) | `ai.rs` | ✅ |
| 7 | إضافة provider metadata (`provider` + `model`) لأحداث `ai:step` | `ai.rs` + `types.ts` | ✅ |
| 8 | تحديث Settings UI بـ Provider Mode selector + per-role model fields + زر Test OpenRouter | `Settings.tsx` + `api.ts` | ✅ |
| 9 | تحويل `pending_confirms` من `std::sync::Mutex` إلى `tokio::sync::Mutex` (Addendum 2.4) | `lib.rs` + `tools.rs` | ✅ |
| 10 | كتابة `docs/PROVIDER_ROUTING.md` | جديد | ✅ |

**معيار الاكتمال (تحقق)**: المستخدم يختار Cloud/Local/Hybrid من الإعدادات، وكل role يوجَّه حسب الوضع مع fallback تلقائي. كل حدث `ai:step` يحمل `provider` و `model` الفعليَّين. Fallback يُصدِر `ai:error` بـ metadata قبل التحويل. `probe_openrouter` يعيد `reachable/key_valid/model_available/credits_remaining`.

---

## ✅ المرحلة 3 — نظام Thinking UI (مُكتملة)

**الهدف**: تحويل الـ chat من "حائط نصوص" إلى واجهة ذكية بثلاث طبقات بصرية.

**المرجع**: `FULL_SYSTEM_AUDIT.md` Section 7.2-7.3

| # | المهمة | الملف | الحالة |
|---|--------|-------|----------|
| 1 | `ThinkingBlock` component (streaming → collapsed → expanded) | `ThinkingBlock.tsx` جديد | ✅ |
| 2 | `FinalAnswerBubble` — tier 1، بارز، دائماً مرئي | `FinalAnswerBubble.tsx` جديد | ✅ |
| 3 | `SystemAction` — tier 3، مصغّر، inline | `SystemAction.tsx` جديد | ✅ |
| 4 | `classifyMessages` — تصنيف الرسائل على ثلاث طبقات | ضمن `Chat.tsx` | ✅ |
| 5 | استبدال rendering في `Chat.tsx` بالنظام الثلاثي | `Chat.tsx` | ✅ |
| 6 | Provider/model badges على كل thinking + final bubble | `Chat.tsx` + CSS | ✅ |
| 7 | `prefers-reduced-motion` يوقف streaming-dot animation | CSS | ✅ |
| 8 | Debounced streaming (50ms batching لـ `ai:token`) | `Chat.tsx` | ⏳ (مؤجَّل لـ Phase 5 Polish) |

**معيار الاكتمال**: رسائل planner/executor/reviewer تظهر كـ thinking blocks قابلة للطي. الإجابة النهائية (آخر executor) بارزة كـ tier-1 bubble. كل bubble يحمل badge للـ provider/model الفعلي القادم من `ai:step`.

---

## ✅ المرحلة 4 — إعادة تصميم Task Panel (مُكتملة)

**الهدف**: جعل الوضع التلقائي (autonomous mode) موثوقاً بصرياً.

**المرجع**: `FULL_SYSTEM_AUDIT.md` Section 7.4

| # | المهمة | الملف | الحالة |
|---|--------|-------|--------|
| 1 | أيقونات حالة (✓/✗/⋯/○/⊘) بدل نص | `TaskPanel.tsx` | ✅ `.task-icon` + `statusIcon()` + goal-level icon في status chip |
| 2 | توقيت لكل task + goal (مدة/elapsed live) | `TaskPanel.tsx` | ✅ `nowSec` tick + `formatDurationSec()` + `.task-duration` + `.task-goal-duration` |
| 3 | شريط تقدم بنسبة مئوية + عداد + running/failed counts | `TaskPanel.tsx` | ✅ `progressbar` ARIA + `.task-progress-running` / `.task-progress-failed` chips |
| 4 | Tool calls مباشرة على الـ task الجاري | `TaskPanel.tsx` | ✅ `pickLiveAction()` + `.task-live-action` (newest tool_call / first error) |
| 5 | Execution pane → Debug panel قابل للطي | `App.tsx` | ✅ `debugOpen` + auto-expand على `ai:error` + `panes-debug-collapsed` grid |
| 6 | ملخصات فشل مختصرة (بدل JSON خام) | `TaskPanel.tsx` | ✅ `condenseResult()` + `shortTaskId()` + `title` لعرض النص الكامل |
| 7 | إظهار `executor_iterations` counter (Addendum 2.9) | `ChatResponse` + UI | ✅ حقل `executor_iterations` على `ChatResponse` في `ai.rs` + typed على `api.ts` |

**معيار الاكتمال**: بنظرة واحدة: كم task اكتمل، أيهم يعمل، كم بقي، وكم استغرق. ✅

**PR**: `feat(desktop): Phase 4 — Task Panel redesign + collapsible Debug pane`

---

## ⏳ المرحلة 5 — تلميع وأداء (1 أسبوع)

**الهدف**: تحويل الأداة من "developer tool" إلى "product".

**المرجع**: `FULL_SYSTEM_AUDIT.md` Section 7.6-7.7

| # | المهمة | الملف | الأولوية |
|---|--------|-------|----------|
| 1 | Dark theme color palette | CSS variables | 🔴 عالية |
| 2 | Inter + JetBrains Mono typography | CSS + package.json | 🟡 متوسطة |
| 3 | 8px grid spacing system | CSS | 🟡 متوسطة |
| 4 | Responsive layout (panels قابلة للطي) | `App.tsx` | 🔴 عالية |
| 5 | Loading skeletons | `Chat.tsx`, `TaskPanel.tsx` | 🟡 متوسطة |
| 6 | Zustand store بدل `useState` chains | `store.ts` جديد | 🔴 عالية |
| 7 | `react-window` virtualization لقائمة الرسائل | `Chat.tsx` | 🟡 متوسطة |
| 8 | Event ring buffer (cap 500 entries) | `App.tsx` | 🟡 متوسطة |
| 9 | ARIA roles + keyboard nav + `prefers-reduced-motion` (Addendum 2.8) | كل الـ components | 🔴 عالية |

**معيار الاكتمال**: هوية بصرية احترافية. يعمل على أحجام شاشات مختلفة. لا jank. WCAG 2.1 AA.

---

## إحصائيات المشاكل

| الخطورة | المرحلة 1 | المرحلة 2 | المرحلة 3 | المرحلة 4 | المرحلة 5 | المجموع |
|---------|-----------|-----------|-----------|-----------|-----------|---------|
| 🔴 Critical | 2 ✅ | 0 | 0 | 0 | 0 | **2** |
| 🟠 High | 4 ✅ | 4 | 5 | 3 | 3 | **19** |
| 🟡 Medium | 4 ✅ | 4 | 2 | 3 | 4 | **17** |
| 🟢 Low | 0 | 2 | 1 | 1 | 0 | **4** |
| **المجموع** | **10 ✅** | **10** | **8** | **7** | **9** | **44** |

---

## الجدول الزمني

```
✅ المرحلة 1 (مُكتملة)  ← استقرار + موثوقية
   الأسبوع 1-2          ← المرحلة 2: توجيه المزودين (العائق المعماري #1)
   الأسبوع 3-4          ← المرحلة 3: Thinking UI (التحسين UX الأكبر)
   الأسبوع 5            ← المرحلة 4: Task Panel (الوضع التلقائي موثوق بصرياً)
   الأسبوع 6            ← المرحلة 5: تلميع + أداء (يحوّل الأداة إلى منتج)
```

**التقدير الإجمالي**: 5-6 أسابيع متبقية للوصول إلى v1.0 قابل للإطلاق.

---

## التقييم المُحدَّث

| البُعد | قبل المرحلة 1 | بعد المرحلة 1 | الهدف (v1.0) |
|--------|---------------|---------------|--------------|
| Architecture | 7.0 | 7.5 | 9.0 |
| Execution Reliability | 6.0 | 7.5 | 8.5 |
| Context Grounding | 7.0 | 8.0 | 8.5 |
| UX Quality | 4.0 | 4.0 | 8.0 |
| Production Readiness | 4.0 | 5.0 | 8.0 |
| **المُركَّب** | **5.6** | **6.4** | **8.4** |

---

---

## تصميم AI Provider Architecture

### الأوضاع الثلاثة

```
Cloud Mode:   Planner → OpenRouter | Executor → OpenRouter | Reviewer → OpenRouter
Local Mode:   Planner → Ollama    | Executor → Ollama    | Reviewer → Ollama
Hybrid Mode:  Planner → OpenRouter | Executor → Ollama    | Reviewer → OpenRouter
```

### Hybrid Routing Strategy

```
┌─────────────────────────────────────────────┐
│              Hybrid Mode                     │
│                                              │
│  Planner   → OpenRouter (strong reasoning)   │
│  Executor  → Ollama (fast, local, 16 iters)  │
│  Reviewer  → OpenRouter (accurate verdict)   │
│                                              │
│  Fallback:                                   │
│  ├─ Ollama fails    → retry 1x → OpenRouter  │
│  ├─ OpenRouter fails → retry 1x → Ollama     │
│  ├─ OpenRouter 429   → backoff 30s → retry   │
│  └─ Both fail        → task fails            │
│                                              │
│  Cost: ~2 cloud calls per task               │
│  (planner + reviewer only)                   │
└─────────────────────────────────────────────┘
```

### Settings Config

```json
{
  "provider_mode": "hybrid",
  "openrouter_api_key": "sk-...",
  "openrouter_model": "anthropic/claude-3.5-sonnet",
  "ollama_base_url": "http://localhost:11434",
  "ollama_model": "deepseek-coder:6.7b",
  "planner_model": "",
  "reviewer_model": "",
  "executor_model": ""
}
```

فارغ = يستخدم الافتراضي حسب الوضع. `planner_model` فارغ في Hybrid = `openrouter_model`.

### Failure Matrix

| السيناريو | Primary | Retry | Fallback | النتيجة |
|-----------|---------|-------|----------|---------|
| Ollama 500 | Ollama | 1× بعد 2s | OpenRouter | task fails إذا فشل الاثنان |
| Ollama offline | Ollama | — | OpenRouter | task fails إذا فشل |
| OpenRouter 429 | OpenRouter | 1× بعد 30s | Ollama | task fails إذا فشل الاثنان |
| OpenRouter 500 | OpenRouter | 1× بعد 5s | Ollama | task fails إذا فشل الاثنان |
| OpenRouter no key | — | — | Ollama | planner يُتخطى |
| Both down | — | — | — | task fails, circuit breaker |

---

## تصميم UI Architecture

### Component Tree

```
App
├── TopBar (ProjectSelector, ProviderStatus, SettingsButton)
├── MainLayout (responsive, collapsible)
│   ├── SidePanel
│   │   ├── Explorer
│   │   └── TaskPanel
│   │       ├── GoalInput
│   │       ├── TaskList → TaskRow (icon, desc, timing)
│   │       ├── ProgressBar
│   │       └── FailureLog (collapsed)
│   ├── ChatPanel (primary)
│   │   ├── MessageList (virtualized)
│   │   │   ├── UserBubble
│   │   │   ├── FinalAnswerBubble (tier 1)
│   │   │   ├── ThinkingBlock (tier 2, collapsible)
│   │   │   └── SystemAction (tier 3, minimal)
│   │   ├── StreamingIndicator
│   │   └── ChatInput
│   └── DebugPanel (collapsed by default)
├── ConfirmCmdOverlay
└── SettingsModal
```

### ThinkingBlock Lifecycle

```
streaming → tokens تظهر بشفافية 60%، tool calls inline
    ↓ (agent completes)
collapsed → سطر واحد ملخص + عدد tool calls + toggle ▸
    ↓ (user clicks ▸)
expanded  → النص الكامل + tool calls مع ✓/✗
    ↓ (user clicks ▾)
collapsed → يرجع للملخص
```

### State Management (Zustand)

```typescript
interface AppStore {
  projectDir: string | null;
  providerStatus: { planner: boolean | null; executor: boolean | null };
  messages: ChatMessage[];          // virtualized
  taskTree: TaskTree | null;
  thinkingStates: Map<string, ThinkingState>;
  events: ExecutionEvent[];         // ring buffer, cap 500
  addMessage: (msg: ChatMessage) => void;
  collapseThinking: (id: string) => void;
  expandThinking: (id: string) => void;
}
```

### Performance

- `react-window` للرسائل (100+ message sessions)
- `React.memo` على `TaskRow`, `ThinkingBlock`, `FinalAnswerBubble`
- Debounced streaming: batch `ai:token` كل 50ms
- `React.lazy` + `Suspense` للـ DebugPanel
- Event ring buffer: cap 500, drop oldest

---

## مراجعة التوثيق

### الحالة الحالية

| الملف | الحالة | المطلوب |
|-------|--------|---------|
| `README.md` | ✅ جيد | — |
| `PROJECT_PLAN.md` | ✅ جيد | — |
| `FULL_SYSTEM_AUDIT.md` | ⚠️ يحتاج تحديث | تحديث الأقسام المُصلحة في المرحلة 1 |
| `FULL_SYSTEM_AUDIT_ADDENDUM.md` | ⚠️ يحتاج تحديث | تحديث items 2.1, 2.5, 2.6 (مُصلحة) |
| `docs/EVALUATION.md` | ❌ قديم | يحتاج إعادة كتابة (مكتوب عند PR #12) |
| `docs/SCENARIOS.md` | ✅ جيد | — |
| `docs/USAGE.md` | ⚠️ جزئي | إضافة screenshots |
| `AGENTS.md` / `CLAUDE.md` | ⚠️ يغطي `src/` فقط | إضافة قسم عن `desktop/` |

### الملفات المطلوب إنشاؤها

| الملف | المرحلة | المحتوى |
|-------|---------|---------|
| `docs/ARCHITECTURE.md` | Phase 2 | agent loop, cancellation, tool safety, memory schema |
| `docs/PROVIDER_ROUTING.md` | Phase 2 | multi-provider system docs |
| `docs/UI_DESIGN.md` | Phase 3 | UI specs, thinking blocks, a11y |

### الهيكل المقترح

```
docs/
├── ARCHITECTURE.md
├── EVALUATION.md          (تحديث)
├── PROVIDER_ROUTING.md    (جديد)
├── SCENARIOS.md
├── USAGE.md               (تحديث)
└── UI_DESIGN.md           (جديد)
```

---

## المشاكل المتبقية بعد المرحلة 1

### مُصلحة ✅

| المشكلة | الخطورة | الإصلاح |
|---------|---------|---------|
| `call_executor_with_fallback` بدون fallback | Critical | fallback حقيقي Ollama → OpenRouter |
| Reviewer يرى نص فقط | Critical | يرى ✓/✗ + output لكل tool call |
| لا context compaction | High | sliding window آخر 20 رسالة |
| `parse_plan_json` يفشل مع two JSON objects | High | balanced bracket counting |
| لا JSON retry في `plan_goal` | High | retry مرة مع reprompt |
| فقدان السياق بين المهام | Medium | ملخص المهام السابقة (500 حرف) |
| `Mutex` بدل `RwLock` للـ settings | Medium | `RwLock` مع `.read()`/`.write()` |

### متبقية ⏳

| المشكلة | الخطورة | المرحلة |
|---------|---------|---------|
| لا `ProviderMode` قابل للإعداد | Critical | 2 |
| Planner معطّل في Local mode | High | 2 |
| لا thinking blocks في الـ chat | High | 3 |
| Execution timeline مسطّح | High | 4 |
| لا `response_format: json_object` | Medium | 1 (متبقي) |
| لا `search_file`/`grep` tool | Medium | 2 |
| `heuristic_split_goal` English-only | Medium | 2 |
| Reviewer يقبل "Looks good!" بدون OK: prefix | Medium | 2 |
| Events array unbounded في App.tsx | Medium | 5 |
| لا loading states | Medium | 5 |
| لا responsive layout | Low | 5 |
| لا ARIA roles / keyboard nav | Medium | 5 |
| `pending_confirms` sync Mutex في async | Medium | 2 |
| Health probe timeout 3s قصير جداً | Low | 2 |
| `check_planner` key-exists فقط، ليس reachability | Low | 2 |

---

## الحكم النهائي

### الحالة بعد المرحلة 1

الأساس **صلب**: طبقة الإلغاء ممتازة، نظام التتبع مصمم جيداً،
الكتابة الذرية للذاكرة آمنة، project context injection يعمل.

إصلاحات المرحلة 1 رفعت **الموثوقية بشكل ملموس**:
- الـ Reviewer الآن يتحقق من الحقائق (لا يمكن لـ executor مهلوس المرور)
- الجلسات الطويلة لن تتجاوز context window
- فشل Ollama لا يوقف كل شيء (fallback حقيقي)
- التخطيط أكثر موثوقية (JSON retry + balanced parsing)

### ما يمنع الإطلاق

1. **لا يوجد توجيه مزودين قابل للإعداد** — المستخدم لا يستطيع اختيار Cloud/Local/Hybrid
2. **واجهة المستخدم مستوى developer** — لا thinking blocks، لا تفرقة بصرية، لا animations
3. **لا يوجد onboarding** — المستخدم يحتاج يقرأ README ليفهم الإعداد

### التوصية

```
المرحلة 2 (توجيه المزودين) ← الأولوية القصوى، العائق المعماري #1
المرحلة 3 (Thinking UI)    ← يحوّل الأداة إلى منتج
المرحلة 4 (Task Panel)     ← يجعل الوضع التلقائي موثوقاً بصرياً
المرحلة 5 (تلميع)          ← يجعلها احترافية
```

**التقييم**: 6.4/10 → الهدف 8.4/10 في 5-6 أسابيع.

**الخلاصة**: ليس جاهزاً للإطلاق العام. **صالح للاستخدام المحلي المراقب**.
الأساس ممتاز والمسار واضح.

---

*هذا الملف يُحدَّث مع كل مرحلة مُكتملة.*
*المرجع الأساسي: `FULL_SYSTEM_AUDIT.md` + `FULL_SYSTEM_AUDIT_ADDENDUM.md`*
