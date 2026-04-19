# خطة التطوير — Development Plan

**تاريخ الإنشاء**: بعد تنفيذ المرحلة 1 (commit `7ca666e`)
**المرجع**: `FULL_SYSTEM_AUDIT.md` + `FULL_SYSTEM_AUDIT_ADDENDUM.md`
**التقييم الحالي**: 6.4 / 10 (كان 5.6 قبل المرحلة 1)

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

## ⏳ المرحلة 2 — نظام توجيه المزودين (1-2 أسبوع)

**الهدف**: تحويل التوجيه الضمني (key present → cloud, else local) إلى نظام صريح قابل للإعداد مع fallback.

**المرجع**: `FULL_SYSTEM_AUDIT.md` Section 6 + Addendum 2.2

| # | المهمة | الملف | الأولوية |
|---|--------|-------|----------|
| 1 | إضافة `ProviderMode` enum (`Cloud` / `Local` / `Hybrid`) | `settings.rs` | 🔴 عالية |
| 2 | إضافة حقول `planner_model`, `reviewer_model`, `executor_model` | `settings.rs` | 🔴 عالية |
| 3 | بناء `call_model` dispatch + `resolve_provider` routing | `ai.rs` | 🔴 عالية |
| 4 | استبدال كل استدعاءات `stream_ollama`/`stream_openrouter` المباشرة بـ `call_model` | `ai.rs` | 🔴 عالية |
| 5 | إضافة `probe_openrouter` (فحص وصول + صلاحية المفتاح) | `ai.rs` جديد | 🟡 متوسطة |
| 6 | رفع timeout الـ health probe من 3s إلى 10s (Addendum 2.3) | `ai.rs:641,672` | 🟢 منخفضة |
| 7 | إضافة provider metadata لأحداث `ai:step` | `ai.rs` | 🟡 متوسطة |
| 8 | تحديث Settings UI بـ Provider Mode selector + per-role model fields | `Settings.tsx` | 🔴 عالية |
| 9 | تحويل `pending_confirms` من `std::sync::Mutex` إلى `tokio::sync::Mutex` (Addendum 2.4) | `lib.rs` + `tools.rs` | 🟡 متوسطة |
| 10 | كتابة `docs/PROVIDER_ROUTING.md` | جديد | 🟡 متوسطة |

**معيار الاكتمال**: المستخدم يختار Cloud/Local/Hybrid من الإعدادات. كل role يوجَّه حسب الوضع مع fallback تلقائي.

---

## ⏳ المرحلة 3 — نظام Thinking UI (1-2 أسبوع)

**الهدف**: تحويل الـ chat من "حائط نصوص" إلى واجهة ذكية بثلاث طبقات بصرية.

**المرجع**: `FULL_SYSTEM_AUDIT.md` Section 7.2-7.3

| # | المهمة | الملف | الأولوية |
|---|--------|-------|----------|
| 1 | `ThinkingBlock` component (streaming → collapsed → expanded) | `ThinkingBlock.tsx` جديد | 🔴 عالية |
| 2 | `FinalAnswerBubble` — tier 1، بارز، دائماً مرئي | `FinalAnswerBubble.tsx` جديد | 🔴 عالية |
| 3 | `SystemAction` — tier 3، مصغّر، inline | `SystemAction.tsx` جديد | 🟡 متوسطة |
| 4 | `generateSummary` — استخراج ملخص client-side | ضمن `ThinkingBlock.tsx` | 🔴 عالية |
| 5 | استبدال rendering في `Chat.tsx` بالنظام الثلاثي | `Chat.tsx` | 🔴 عالية |
| 6 | Collapse/expand animations (200ms ease-out) | CSS | 🟡 متوسطة |
| 7 | Debounced streaming (50ms batching لـ `ai:token`) | `Chat.tsx` | 🟡 متوسطة |
| 8 | إصلاح double-emit bubble على الأجهزة البطيئة (Addendum 2.7) | `Chat.tsx` | 🟢 منخفضة |

**معيار الاكتمال**: رسائل planner/executor تظهر كـ thinking blocks قابلة للطي. الإجابة النهائية بارزة.

---

## ⏳ المرحلة 4 — إعادة تصميم Task Panel (1 أسبوع)

**الهدف**: جعل الوضع التلقائي (autonomous mode) موثوقاً بصرياً.

**المرجع**: `FULL_SYSTEM_AUDIT.md` Section 7.4

| # | المهمة | الملف | الأولوية |
|---|--------|-------|----------|
| 1 | أيقونات حالة (✓/✗/⋯/○/⊘) بدل نص | `TaskPanel.tsx` | 🔴 عالية |
| 2 | توقيت لكل task (مدة للمكتمل / "running..." للجاري) | `TaskPanel.tsx` | 🔴 عالية |
| 3 | شريط تقدم بنسبة مئوية + عداد (2/5 tasks) | `TaskPanel.tsx` | 🔴 عالية |
| 4 | Tool calls مباشرة على الـ task الجاري | `TaskPanel.tsx` | 🟡 متوسطة |
| 5 | Execution pane → Debug panel قابل للطي | `App.tsx` | 🟡 متوسطة |
| 6 | ملخصات فشل مختصرة (بدل JSON خام) | `TaskPanel.tsx` | 🟡 متوسطة |
| 7 | إظهار `executor_iterations` counter (Addendum 2.9) | `ChatResponse` + UI | 🟢 منخفضة |

**معيار الاكتمال**: بنظرة واحدة: كم task اكتمل، أيهم يعمل، كم بقي، وكم استغرق.

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

*هذا الملف يُحدَّث مع كل مرحلة مُكتملة.*
*المرجع الأساسي: `FULL_SYSTEM_AUDIT.md` + `FULL_SYSTEM_AUDIT_ADDENDUM.md`*
