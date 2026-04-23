import { useEffect, useMemo, useState } from "react";
import { api } from "../api";
import { useAppStore } from "../store";
import type { Settings } from "../types";

import openRouterCategorizedModelsCsv from "../../../../OpenRouter_Categorized_Models.csv?raw";

/**
 * Heuristic: does the model name look like a ≤3B parameter model?
 *
 * We match `1b`, `1.5b`, `2b`, `3b` (and the common hyphen/colon tag
 * separators Ollama and OpenRouter use — `llama3.2:1b`, `phi3-mini-3b`,
 * `gemma-2b-it`, etc.). This is deliberately permissive on the low end
 * and restrictive on the high end: a false negative (miss a 4B as
 * small) is fine, a false positive (warn on a 7B) would train users to
 * ignore the warning.
 *
 * Why only ≤3B: Scenario A (§9.2 of `PROJECT_MEMORY.md`) reproduced
 * `review skipped (unparsed)` on `llama3.2:1b`. Models in the 1-3B
 * range consistently fail to emit tool_calls for the executor. 7B+
 * models (qwen2.5-coder:7b, llama3.1:8b) pass the same goal reliably.
 *
 * Only applied to executor and planner slots — reviewer tolerates
 * small models because its output is a short verdict, not structured
 * tool calls.
 */
export function modelLooksSmall(name: string | null | undefined): boolean {
  if (!name) return false;
  // Match one of: "1b", "1.5b", "2b", "3b" preceded by a non-alnum
  // boundary (`:`, `-`, `_`, `.`, space) and followed by a word
  // boundary or end-of-string. Case-insensitive.
  return /(?:^|[^a-z0-9])(?:1(?:\.[05])?|2|3)\s*b(?:$|[^a-z0-9])/i.test(name);
}

const DEFAULTS: Settings = {
  openrouter_api_key: "",
  openrouter_model: "openrouter/auto",
  ollama_base_url: "http://localhost:11434",
  ollama_model: "deepseek-coder:6.7b",
  // Default to `hybrid` — most users have both an OpenRouter key and a
  // local Ollama; the backend downgrades this to `local` on first boot
  // when no key is present, so this default is safe either way.
  provider_mode: "hybrid",
  planner_model: "",
  reviewer_model: "",
  executor_model: "",
  reviewer_enabled: true,
  max_iterations: 8,
  cmd_confirm_required: true,
  cmd_allow_list: [],
  autonomous_mode: false,
  max_retries_per_task: 3,
  max_total_tasks: 20,
  task_timeout_secs: 180,
  goal_timeout_secs: 3600,
  retry_backoff_base_ms: 1000,
  circuit_breaker_threshold: 5,
  max_parallel_tasks: 1,
  autonomous_confirm_irreversible: false,
  context_compaction_enabled: false,
  context_compaction_keep_last: 20,
};

type ProbeState =
  | { kind: "idle" }
  | { kind: "testing" }
  | {
      kind: "ok";
      model_available: boolean;
      available_models: string[];
    }
  | { kind: "err"; message: string };

/**
 * Probe state for OpenRouter. Separate from `ProbeState` because the
 * OpenRouter probe also reports `key_valid` and optional
 * `credits_remaining` that Ollama does not expose.
 */
type OpenRouterProbeState =
  | { kind: "idle" }
  | { kind: "testing" }
  | {
      kind: "ok";
      key_valid: boolean;
      model_available: boolean;
      available_models: string[];
      credits_remaining: number | null;
    }
  | { kind: "err"; message: string };

type OpenRouterCsvModel = {
  name: string;
  category: string;
  provider: string;
  url: string;
  modelId: string;
};

type CuratedModel = {
  label: string;
  modelId: string;
  description: string;
};

type CuratedCategory = {
  title: string;
  description: string;
  models: CuratedModel[];
};

const OPENROUTER_FREE_CATEGORIES: CuratedCategory[] = [
  {
    title: "الفئة الأولى: الموديلات العملاقة (القمة في البرمجة)",
    description:
      "هذه الموديلات تمتلك أكبر عدد من المعلمات وتعتبر الأفضل في حل المشكلات البرمجية المعقدة:",
    models: [
      {
        label: "Qwen3 Coder 480B A35B (free)",
        modelId: "qwen/qwen3-coder:free",
        description:
          "الأفضل على الإطلاق في القائمة للكود بناءً على تخصص عائلة Qwen Coder.",
      },
      {
        label: "Hermes 3 405B Instruct (free)",
        modelId: "nousresearch/hermes-3-llama-3.1-405b:free",
        description:
          "مبني على Llama 3 405B، وهو أضخم موديل مفتوح المصدر ويمتلك قدرات استدلال هائلة.",
      },
      {
        label: "Qwen3 Next 80B A3B Instruct (free)",
        modelId: "qwen/qwen3-next-80b-a3b-instruct:free",
        description: "موديل جيل قادم من Qwen، يتفوق في الرياضيات والكود.",
      },
      {
        label: "Llama 3.3 70B Instruct (free)",
        modelId: "meta-llama/llama-3.3-70b-instruct:free",
        description:
          "المعيار الذهبي الحالي للموديلات ذات الحجم المتوسط في البرمجة.",
      },
    ],
  },
  {
    title: "الفئة الثانية: الموديلات المتقدمة (قوية جداً)",
    description: "",
    models: [
      {
        label: "GLM 4.5 Air (free)",
        modelId: "z-ai/glm-4.5-air:free",
        description: "موديل صيني متطور جداً وينافس GPT-4 في المهام المنطقية.",
      },
      {
        label: "Kimi K2.6",
        modelId: "moonshotai/kimi-k2.6",
        description:
          "موديل قوي جداً في السياقات الطويلة وفهم بنية الأكواد الضخمة.",
      },
      {
        label: "Gemma 4 31B (free)",
        modelId: "google/gemma-4-31b-it:free",
        description:
          "جيل قادم من جوجل، متوقع أن يكون متفوقاً جداً في الكود مقارنة بحجمه.",
      },
      {
        label: "Gemma 4 26B A4B (free)",
        modelId: "google/gemma-4-26b-a4b-it:free",
        description: "نسخة أخف قليلاً من Gemma 4 ولكنها ذكية جداً.",
      },
      {
        label: "gpt-oss-120b (free)",
        modelId: "openai/gpt-oss-120b:free",
        description:
          "بسبب حجمه الكبير 120B، يمتلك قدرة عالية على فهم اللغات البرمجية المتعددة.",
      },
    ],
  },
  {
    title: "الفئة الثالثة: الموديلات المتوسطة (ممتازة للمهام السريعة)",
    description: "",
    models: [
      {
        label: "Gemma 3 27B (free)",
        modelId: "google/gemma-3-27b-it:free",
        description: "",
      },
      {
        label: "Nemotron 3 Super (free)",
        modelId: "nvidia/nemotron-3-super-120b-a12b:free",
        description: "موديلات Nvidia ممتازة في صياغة الأكواد المنطقية.",
      },
      {
        label: "Riverflow V2 Max Preview",
        modelId: "sourceful/riverflow-v2-max-preview",
        description: "",
      },
      {
        label: "MiniMax M2.5 (free)",
        modelId: "minimax/minimax-m2.5:free",
        description: "",
      },
      {
        label: "Trinity Large Preview (free)",
        modelId: "arcee-ai/trinity-large-preview:free",
        description: "",
      },
      {
        label: "Uncensored (free)",
        modelId:
          "cognitivecomputations/dolphin-mistral-24b-venice-edition:free",
        description:
          "غالباً ما يكون مبني على Llama، جيد للكود لكنه يفتقر أحياناً لضوابط الدقة.",
      },
    ],
  },
  {
    title: "الفئة الرابعة: موديلات الحجم الصغير (للكود البسيط والتصحيح)",
    description: "",
    models: [
      {
        label: "Gemma 3 12B (free)",
        modelId: "google/gemma-3-12b-it:free",
        description: "",
      },
      {
        label: "Nemotron 3 Nano 30B A3B (free)",
        modelId: "nvidia/nemotron-3-nano-30b-a3b:free",
        description: "",
      },
      {
        label: "Riverflow V2 Pro",
        modelId: "sourceful/riverflow-v2-pro",
        description: "",
      },
      {
        label: "Riverflow V2 Standard Preview",
        modelId: "sourceful/riverflow-v2-standard-preview",
        description: "",
      },
      {
        label: "gpt-oss-20b (free)",
        modelId: "openai/gpt-oss-20b:free",
        description: "",
      },
      {
        label: "Lyria 3 Pro Preview",
        modelId: "google/lyria-3-pro-preview",
        description:
          "موديل من جوجل، يميل أكثر للوسائط المتعددة لكنه جيد في البرمجة الأساسية.",
      },
      {
        label: "Gemma 3n 4B (free)",
        modelId: "google/gemma-3n-e4b-it:free",
        description: "",
      },
      {
        label: "Gemma 3 4B (free)",
        modelId: "google/gemma-3-4b-it:free",
        description: "",
      },
      {
        label: "Llama 3.2 3B Instruct (free)",
        modelId: "meta-llama/llama-3.2-3b-instruct:free",
        description: "صغير جداً، يصلح فقط للأكواد البسيطة جداً.",
      },
      {
        label: "LFM2.5-1.2B-Thinking (free)",
        modelId: "liquid/lfm-2.5-1.2b-thinking:free",
        description: "موديل يعتمد على التفكير العميق رغم صغر حجمه.",
      },
    ],
  },
  {
    title: "الفئة الخامسة: موديلات متخصصة أو غير معروفة كلياً في البرمجة",
    description: "",
    models: [
      {
        label: "Elephant",
        modelId: "openrouter/elephant-alpha",
        description: "",
      },
      {
        label: "Riverflow V2 Fast",
        modelId: "sourceful/riverflow-v2-fast",
        description: "",
      },
      {
        label: "LFM2.5-1.2B-Instruct (free)",
        modelId: "liquid/lfm-2.5-1.2b-instruct:free",
        description: "",
      },
      {
        label: "Nemotron Nano 12B 2 VL (free)",
        modelId: "nvidia/nemotron-nano-12b-v2-vl:free",
        description: "",
      },
      {
        label: "Nemotron Nano 9B V2 (free)",
        modelId: "nvidia/nemotron-nano-9b-v2:free",
        description: "",
      },
      {
        label: "Gemma 3n 2B (free)",
        modelId: "google/gemma-3n-e2b-it:free",
        description: "",
      },
      {
        label: "Lyria 3 Clip Preview",
        modelId: "google/lyria-3-clip-preview",
        description: "",
      },
    ],
  },
  {
    title: "الفئة السادسة: موديلات صور (لا تستخدم للبرمجة)",
    description:
      "هذه الموديلات لتوليد الصور فقط، وإذا طلبت منها كوداً فلن تعطيك نتائج مفيدة:",
    models: [
      {
        label: "FLUX.2 Max",
        modelId: "black-forest-labs/flux.2-max",
        description: "",
      },
      {
        label: "FLUX.2 Pro",
        modelId: "black-forest-labs/flux.2-pro",
        description: "",
      },
      {
        label: "FLUX.2 Flex",
        modelId: "black-forest-labs/flux.2-flex",
        description: "",
      },
      {
        label: "FLUX.2 Klein 4B",
        modelId: "black-forest-labs/flux.2-klein-4b",
        description: "",
      },
      {
        label: "Seedream 4.5",
        modelId: "bytedance-seed/seedream-4.5",
        description: "",
      },
    ],
  },
];

function parseCsvRow(row: string): string[] {
  const out: string[] = [];
  let cur = "";
  let inQuotes = false;

  for (let i = 0; i < row.length; i++) {
    const ch = row[i];
    if (ch === '"') {
      if (inQuotes && row[i + 1] === '"') {
        cur += '"';
        i++;
      } else {
        inQuotes = !inQuotes;
      }
      continue;
    }
    if (ch === "," && !inQuotes) {
      out.push(cur);
      cur = "";
      continue;
    }
    cur += ch;
  }
  out.push(cur);
  return out.map((s) => s.trim());
}

function parseOpenRouterCsv(csv: string): OpenRouterCsvModel[] {
  const lines = csv
    .split(/\r?\n/)
    .map((l) => l.trim())
    .filter((l) => l.length > 0);
  if (lines.length === 0) return [];

  const header = parseCsvRow(lines[0]);
  const idxName = header.findIndex((h) => h === "اسم الموديل");
  const idxCategory = header.findIndex((h) => h === "التصنيف");
  const idxProvider = header.findIndex((h) => h === "الموفر (Provider)");
  const idxUrl = header.findIndex((h) => h === "الرابط المباشر");
  if (
    idxName === -1 ||
    idxCategory === -1 ||
    idxProvider === -1 ||
    idxUrl === -1
  ) {
    return [];
  }

  const models: OpenRouterCsvModel[] = [];
  for (const line of lines.slice(1)) {
    const row = parseCsvRow(line);
    const name = row[idxName] ?? "";
    const category = row[idxCategory] ?? "";
    const provider = row[idxProvider] ?? "";
    const url = row[idxUrl] ?? "";
    const modelId = url
      .replace(/^https?:\/\/openrouter\.ai\//i, "")
      .trim()
      .replace(/^\//, "");
    if (!name || !url || !modelId) continue;
    models.push({ name, category, provider, url, modelId });
  }
  return models;
}

function OpenRouterModelBrowser({
  value,
  catalog,
  onPick,
}: {
  value: string;
  catalog: string[] | null;
  onPick: (next: string) => void;
}) {
  const [query, setQuery] = useState("");
  const [category, setCategory] = useState<string>("");
  const [tab, setTab] = useState<"all" | "free">("all");

  const allModels = useMemo(
    () => parseOpenRouterCsv(openRouterCategorizedModelsCsv),
    [],
  );

  const categories = useMemo(() => {
    const set = new Set<string>();
    for (const m of allModels) {
      if (m.category) set.add(m.category);
    }
    return Array.from(set).sort((a, b) => a.localeCompare(b));
  }, [allModels]);

  const normalizedQuery = query.trim().toLowerCase();
  const catalogSet = useMemo(() => {
    if (!catalog || catalog.length === 0) return null;
    return new Set(catalog);
  }, [catalog]);

  const filtered = useMemo(() => {
    return allModels
      .filter((m) => {
        if (tab === "free") return false;
        if (category && m.category !== category) return false;
        if (!normalizedQuery) return true;
        return (
          m.name.toLowerCase().includes(normalizedQuery) ||
          m.modelId.toLowerCase().includes(normalizedQuery)
        );
      })
      .slice(0, 250);
  }, [allModels, category, tab, normalizedQuery]);

  return (
    <div className="or-browser" role="region" aria-label="OpenRouter model browser">
      <div className="or-browser-tabs" role="tablist" aria-label="Model list tabs">
        <button
          type="button"
          role="tab"
          aria-selected={tab === "all"}
          className={`or-browser-tab ${tab === "all" ? "is-active" : ""}`}
          onClick={() => setTab("all")}
        >
          All models
        </button>
        <button
          type="button"
          role="tab"
          aria-selected={tab === "free"}
          className={`or-browser-tab ${tab === "free" ? "is-active" : ""}`}
          onClick={() => setTab("free")}
        >
          Free models
        </button>
      </div>

      <div className="or-browser-controls">
        <input
          className="or-browser-search"
          placeholder={tab === "free" ? "Free list" : "Search models…"}
          value={query}
          disabled={tab === "free"}
          onChange={(e) => setQuery(e.target.value)}
        />
        <select
          className="or-browser-select"
          value={category}
          disabled={tab === "free"}
          onChange={(e) => setCategory(e.target.value)}
          aria-label="Filter by category"
        >
          <option value="">All categories</option>
          {categories.map((c) => (
            <option key={c} value={c}>
              {c}
            </option>
          ))}
        </select>
        <div className="or-browser-count" aria-label="Result count">
          {tab === "free" ? "Curated" : `${filtered.length} shown`}
        </div>
      </div>

      {tab === "free" ? (
        <div className="or-free" role="list">
          {OPENROUTER_FREE_CATEGORIES.map((cat) => (
            <div key={cat.title} className="or-free-category" role="listitem">
              <div className="or-free-category-title">{cat.title}</div>
              {cat.description && (
                <div className="or-free-category-desc">{cat.description}</div>
              )}
              <div className="or-free-items">
                {cat.models.map((m) => {
                  const available = catalogSet ? catalogSet.has(m.modelId) : null;
                  return (
                    <button
                      key={m.modelId}
                      type="button"
                      className={`or-free-item ${m.modelId === value ? "is-active" : ""}`}
                      onClick={() => onPick(m.modelId)}
                      title={m.modelId}
                    >
                      <div className="or-free-item-main">
                        <div className="or-free-item-title">{m.label}</div>
                        <div className="or-free-item-sub">{m.modelId}</div>
                        {m.description && (
                          <div className="or-free-item-desc">{m.description}</div>
                        )}
                      </div>
                      <div className="or-free-item-meta">
                        {available === true && (
                          <span className="or-browser-chip or-ok">In catalog</span>
                        )}
                        {available === false && (
                          <span className="or-browser-chip or-warn">Not in catalog</span>
                        )}
                      </div>
                    </button>
                  );
                })}
              </div>
            </div>
          ))}
        </div>
      ) : (
        <div className="or-browser-list" role="list">
          {filtered.map((m) => {
            const available = catalogSet ? catalogSet.has(m.modelId) : null;
            return (
              <button
                key={m.modelId}
                type="button"
                className={`or-browser-item ${m.modelId === value ? "is-active" : ""}`}
                onClick={() => onPick(m.modelId)}
                title={m.modelId}
                role="listitem"
              >
                <div className="or-browser-item-main">
                  <div className="or-browser-item-title">{m.name}</div>
                  <div className="or-browser-item-sub">{m.modelId}</div>
                </div>
                <div className="or-browser-item-meta">
                  {m.category && (
                    <span className="or-browser-badge">{m.category}</span>
                  )}
                  {available === true && (
                    <span className="or-browser-chip or-ok">In catalog</span>
                  )}
                  {available === false && (
                    <span className="or-browser-chip or-warn">Not in catalog</span>
                  )}
                </div>
              </button>
            );
          })}
        </div>
      )}
    </div>
  );
}

export function SettingsModal({ onClose }: { onClose: () => void }) {
  const [s, setS] = useState<Settings>(DEFAULTS);
  const [loaded, setLoaded] = useState(false);
  const [saving, setSaving] = useState(false);
  const [allowListText, setAllowListText] = useState("");
  // OC-Titan §VI.2/§VI.3 — dev-mode toggle. Persisted locally (not on
  // the backend settings blob) so it survives restarts without the
  // user having to re-accept the "opt-in" gates on every launch.
  const devMode = useAppStore((st) => st.devMode);
  const setDevMode = useAppStore((st) => st.setDevMode);
  const [probe, setProbe] = useState<ProbeState>({ kind: "idle" });
  const [orProbe, setOrProbe] = useState<OpenRouterProbeState>({
    kind: "idle",
  });
  const [openRouterCatalog, setOpenRouterCatalog] = useState<string[] | null>(
    null,
  );
  const [openRouterBrowserOpen, setOpenRouterBrowserOpen] = useState(false);

  const testOpenRouter = async () => {
    setOrProbe({ kind: "testing" });
    try {
      const r = await api.probeOpenrouter(
        s.openrouter_api_key,
        s.openrouter_model,
      );
      if (!r.reachable) {
        setOrProbe({
          kind: "err",
          message: r.error ?? "OpenRouter is not reachable.",
        });
        return;
      }
      setOpenRouterCatalog(r.available_models ?? []);
      setOrProbe({
        kind: "ok",
        key_valid: r.key_valid,
        model_available: r.model_available,
        credits_remaining: r.credits_remaining,
        available_models: r.available_models ?? [],
      });
    } catch (e) {
      setOrProbe({ kind: "err", message: String(e) });
    }
  };

  const testOllama = async () => {
    setProbe({ kind: "testing" });
    try {
      const r = await api.probeOllama(s.ollama_base_url, s.ollama_model);
      if (!r.reachable) {
        setProbe({
          kind: "err",
          message:
            r.error ??
            "Ollama is not reachable. Is `ollama serve` running at the configured URL?",
        });
        return;
      }
      setProbe({
        kind: "ok",
        model_available: r.model_available,
        available_models: r.available_models,
      });
    } catch (e) {
      setProbe({ kind: "err", message: String(e) });
    }
  };

  useEffect(() => {
    void (async () => {
      try {
        const cur = await api.getSettings();
        const merged = { ...DEFAULTS, ...cur };
        // OC-Titan §VI.2/§VI.3 — if the user previously enabled
        // dev-mode, surface the three backend gates as ON even when
        // the persisted backend settings are still at their opt-out
        // defaults. The flags are only written to disk when the user
        // hits Save, so this is a pure display reconciliation — it
        // restores the previous dev-mode intent without losing the
        // user-saved values for any other field.
        if (devMode) {
          merged.autoinstall_enabled = true;
          merged.security_gate_execute_enabled = true;
          merged.runtime_validation_enabled = true;
        }
        setS(merged);
        setAllowListText(merged.cmd_allow_list.join("\n"));
      } catch {
        /* use defaults */
      } finally {
        setLoaded(true);
      }
    })();
    // devMode is read once on open: the toggle directly mutates `s`
    // at click-time, so we don't need to re-sync on every change.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const save = async () => {
    setSaving(true);
    try {
      // Coerce a possibly-non-numeric setting to a finite integer. We cannot
      // use `Number(x) || default` because `Number(0)` is falsy and 0 is a
      // valid user input (it means "disabled" for timeout / circuit-breaker
      // settings). NaN (empty string, garbage) falls back to `fallback`.
      const num = (v: unknown, fallback: number): number => {
        const n = Number(v);
        return Number.isFinite(n) ? n : fallback;
      };
      const normalized: Settings = {
        ...s,
        max_iterations: Math.max(1, Math.min(16, num(s.max_iterations, 8))),
        max_retries_per_task: Math.max(
          0,
          Math.min(10, num(s.max_retries_per_task, 3)),
        ),
        max_total_tasks: Math.max(
          1,
          Math.min(100, num(s.max_total_tasks, 20)),
        ),
        task_timeout_secs: Math.max(
          0,
          Math.min(3600, num(s.task_timeout_secs, 180)),
        ),
        goal_timeout_secs: Math.max(
          0,
          Math.min(86400, num(s.goal_timeout_secs, 3600)),
        ),
        retry_backoff_base_ms: Math.max(
          0,
          Math.min(30000, num(s.retry_backoff_base_ms, 1000)),
        ),
        circuit_breaker_threshold: Math.max(
          0,
          Math.min(100, num(s.circuit_breaker_threshold, 5)),
        ),
        max_parallel_tasks: Math.max(
          1,
          Math.min(8, num(s.max_parallel_tasks, 1)),
        ),
        cmd_allow_list: allowListText
          .split("\n")
          .map((l) => l.trim())
          .filter((l) => l.length > 0),
      };
      await api.saveSettings(normalized);
      onClose();
    } finally {
      setSaving(false);
    }
  };

  if (!loaded) {
    return (
      <div className="settings-overlay">
        <div className="settings-modal">Loading…</div>
      </div>
    );
  }
  return (
    <div className="settings-overlay" onClick={onClose}>
      <div className="settings-modal" onClick={(e) => e.stopPropagation()}>
        <div style={{ display: "flex", justifyContent: "space-between", gap: 12 }}>
          <h2>Settings</h2>
          <div style={{ fontSize: 10, color: "#777", paddingTop: 6 }}>
            build <code>{__BUILD_STAMP__}</code>
          </div>
        </div>

        <div className="settings-card">
          <div className="settings-card-header">
            <div className="settings-card-title">OpenRouter</div>
            <div className="settings-card-subtitle">
              Cloud / Hybrid configuration
            </div>
          </div>

          <div className="row">
            <label>
              Provider mode
              <span
                style={{ color: "#8a8a8a", fontSize: 11, marginLeft: 6 }}
              >
                — how planner, executor, reviewer map onto backends
              </span>
            </label>
            <select
              value={s.provider_mode}
              onChange={(e) =>
                setS({
                  ...s,
                  provider_mode: e.target.value as Settings["provider_mode"],
                })
              }
            >
              <option value="cloud">
                Cloud (everything on OpenRouter, no fallback)
              </option>
              <option value="local">
                Local (everything on Ollama, no fallback)
              </option>
              <option value="hybrid">
                Hybrid (planner/reviewer on OpenRouter, executor on Ollama,
                cross-fallback)
              </option>
            </select>
          </div>

          <div className="row">
            <label>OpenRouter API key</label>
            <input
              type="password"
              placeholder="sk-or-…"
              value={s.openrouter_api_key}
              onChange={(e) =>
                setS({ ...s, openrouter_api_key: e.target.value })
              }
            />
          </div>

          <div className="row">
            <label>OpenRouter default model</label>
            <input
              value={s.openrouter_model}
              onChange={(e) => setS({ ...s, openrouter_model: e.target.value })}
            />
            <div style={{ display: "flex", justifyContent: "flex-end" }}>
              <button
                type="button"
                className="or-browser-toggle"
                onClick={() => setOpenRouterBrowserOpen((v) => !v)}
              >
                {openRouterBrowserOpen ? "Hide model browser" : "Browse models"}
              </button>
            </div>
            {openRouterBrowserOpen && (
              <OpenRouterModelBrowser
                value={s.openrouter_model}
                catalog={openRouterCatalog}
                onPick={(next) => {
                  setS({ ...s, openrouter_model: next });
                  setOpenRouterBrowserOpen(false);
                }}
              />
            )}
          </div>

          <div className="row">
            <div style={{ display: "flex", alignItems: "center", gap: 8 }}>
              <button
                type="button"
                onClick={testOpenRouter}
                disabled={orProbe.kind === "testing"}
              >
                {orProbe.kind === "testing"
                  ? "Testing…"
                  : "Test OpenRouter connection"}
              </button>
              {orProbe.kind === "ok" && orProbe.key_valid && (
                <span style={{ color: "#4caf50", fontSize: 12 }}>
                  ✓ key valid ·{" "}
                  {orProbe.model_available ? (
                    <>
                      model <code>{s.openrouter_model}</code> available
                    </>
                  ) : (
                    <span style={{ color: "#f9a825" }}>
                      ⚠ model <code>{s.openrouter_model}</code> not in catalog
                    </span>
                  )}
                  {orProbe.credits_remaining !== null && (
                    <> · ${orProbe.credits_remaining.toFixed(2)} left</>
                  )}
                </span>
              )}
              {orProbe.kind === "ok" && !orProbe.key_valid && (
                <span style={{ color: "#ef5350", fontSize: 12 }}>
                  ✗ reachable, but API key was rejected
                </span>
              )}
              {orProbe.kind === "err" && (
                <span style={{ color: "#ef5350", fontSize: 12 }}>
                  ✗ {orProbe.message}
                </span>
              )}
            </div>
          </div>

          <div className="row">
            <label>
              Per-role model overrides (optional — blank = use provider default)
            </label>
            <div
              style={{
                display: "grid",
                gridTemplateColumns: "80px 1fr",
                gap: 6,
                alignItems: "center",
              }}
            >
              <span style={{ fontSize: 12, color: "#bbb" }}>Planner</span>
              <input
                placeholder="e.g. anthropic/claude-3.5-sonnet"
                value={s.planner_model}
                onChange={(e) => setS({ ...s, planner_model: e.target.value })}
              />
              {modelLooksSmall(s.planner_model) && (
                <>
                  <span />
                  <SmallModelWarning role="planner" />
                </>
              )}
              <span style={{ fontSize: 12, color: "#bbb" }}>Reviewer</span>
              <input
                placeholder="e.g. anthropic/claude-3.5-sonnet"
                value={s.reviewer_model}
                onChange={(e) =>
                  setS({ ...s, reviewer_model: e.target.value })
                }
              />
            </div>
          </div>
        </div>

        <div className="settings-card">
          <div className="settings-card-header">
            <div className="settings-card-title">Ollama</div>
            <div className="settings-card-subtitle">Local provider settings</div>
          </div>

          <div className="row">
            <label>Ollama base URL</label>
            <input
              value={s.ollama_base_url}
              onChange={(e) => setS({ ...s, ollama_base_url: e.target.value })}
            />
          </div>
          <div className="row">
            <label>Ollama model (executor)</label>
            <input
              value={s.ollama_model}
              onChange={(e) => setS({ ...s, ollama_model: e.target.value })}
            />
            {modelLooksSmall(s.ollama_model) && (
              <SmallModelWarning role="executor" />
            )}
          </div>

          <div className="row">
            <label>Executor override (optional — blank = use provider default)</label>
            <input
              placeholder="e.g. qwen2.5-coder:7b"
              value={s.executor_model}
              onChange={(e) => setS({ ...s, executor_model: e.target.value })}
            />
            {modelLooksSmall(s.executor_model) && (
              <SmallModelWarning role="executor" />
            )}
          </div>

          <div className="row">
            <div style={{ display: "flex", alignItems: "center", gap: 8 }}>
              <button
                type="button"
                onClick={testOllama}
                disabled={probe.kind === "testing"}
              >
                {probe.kind === "testing"
                  ? "Testing…"
                  : "Test Ollama connection"}
              </button>
              {probe.kind === "ok" && probe.model_available && (
                <span style={{ color: "#4caf50", fontSize: 12 }}>
                  ✓ reachable · model <code>{s.ollama_model}</code> available
                </span>
              )}
              {probe.kind === "ok" && !probe.model_available && (
                <span style={{ color: "#f9a825", fontSize: 12 }}>
                  ⚠ reachable, but model <code>{s.ollama_model}</code> is not
                  pulled. Available:{" "}
                  {probe.available_models.slice(0, 5).join(", ") || "(none)"}
                  {probe.available_models.length > 5 ? ", …" : ""}
                </span>
              )}
              {probe.kind === "err" && (
                <span style={{ color: "#ef5350", fontSize: 12 }}>
                  ✗ {probe.message}
                </span>
              )}
            </div>
          </div>
        </div>

        <div className="row">
          <label>Max executor iterations per turn (1–16)</label>
          <input
            type="number"
            min={1}
            max={16}
            value={s.max_iterations}
            onChange={(e) =>
              setS({ ...s, max_iterations: Number(e.target.value) })
            }
          />
        </div>

        <div className="row">
          <label>
            <input
              type="checkbox"
              checked={s.autonomous_mode}
              onChange={(e) =>
                setS({ ...s, autonomous_mode: e.target.checked })
              }
              style={{ width: "auto", marginRight: 6 }}
            />
            Autonomous mode (goal loop runs without per-task confirmation)
          </label>
        </div>

        <div className="row">
          <label>
            <input
              type="checkbox"
              checked={s.autonomous_confirm_irreversible}
              onChange={(e) =>
                setS({
                  ...s,
                  autonomous_confirm_irreversible: e.target.checked,
                })
              }
              style={{ width: "auto", marginRight: 6 }}
            />
            Confirm irreversible ops in autonomous mode
            <span
              style={{
                color: "#8a8a8a",
                fontSize: 11,
                marginLeft: 6,
              }}
            >
              (prompts for every <code>run_cmd</code> and destructive{" "}
              <code>write_file</code>, bypassing the allow-list)
            </span>
          </label>
        </div>

        <div className="row">
          <label>
            <input
              type="checkbox"
              checked={s.context_compaction_enabled}
              onChange={(e) =>
                setS({
                  ...s,
                  context_compaction_enabled: e.target.checked,
                })
              }
              style={{ width: "auto", marginRight: 6 }}
            />
            Compact chat history in long sessions
            <span
              style={{
                color: "#8a8a8a",
                fontSize: 11,
                marginLeft: 6,
              }}
            >
              (drops the oldest messages past the window below; no
              summary)
            </span>
          </label>
        </div>
        <div className="row">
          <label>Keep last N history messages (≥ 2)</label>
          <input
            type="number"
            min={2}
            max={200}
            value={s.context_compaction_keep_last}
            disabled={!s.context_compaction_enabled}
            onChange={(e) =>
              setS({
                ...s,
                context_compaction_keep_last: Math.max(
                  2,
                  Number(e.target.value) || 20,
                ),
              })
            }
          />
        </div>

        <div className="row">
          <label>Max retries per task (0–10)</label>
          <input
            type="number"
            min={0}
            max={10}
            value={s.max_retries_per_task}
            onChange={(e) =>
              setS({ ...s, max_retries_per_task: Number(e.target.value) })
            }
          />
        </div>
        <div className="row">
          <label>Max total tasks per goal (1–100)</label>
          <input
            type="number"
            min={1}
            max={100}
            value={s.max_total_tasks}
            onChange={(e) =>
              setS({ ...s, max_total_tasks: Number(e.target.value) })
            }
          />
        </div>

        <div className="row">
          <label>Per-task timeout (seconds, 0 disables)</label>
          <input
            type="number"
            min={0}
            max={3600}
            value={s.task_timeout_secs}
            onChange={(e) =>
              setS({ ...s, task_timeout_secs: Number(e.target.value) })
            }
          />
        </div>
        <div className="row">
          <label>Global goal timeout (seconds, 0 disables)</label>
          <input
            type="number"
            min={0}
            max={86400}
            value={s.goal_timeout_secs}
            onChange={(e) =>
              setS({ ...s, goal_timeout_secs: Number(e.target.value) })
            }
          />
        </div>
        <div className="row">
          <label>Retry backoff base (ms, exponential, capped at 30s)</label>
          <input
            type="number"
            min={0}
            max={30000}
            value={s.retry_backoff_base_ms}
            onChange={(e) =>
              setS({ ...s, retry_backoff_base_ms: Number(e.target.value) })
            }
          />
        </div>
        <div className="row">
          <label>Circuit breaker threshold (consecutive failures, 0 disables)</label>
          <input
            type="number"
            min={0}
            max={100}
            value={s.circuit_breaker_threshold}
            onChange={(e) =>
              setS({ ...s, circuit_breaker_threshold: Number(e.target.value) })
            }
          />
        </div>
        <div className="row">
          <label>Max parallel tasks (1 today; &gt;1 reserved for future)</label>
          <input
            type="number"
            min={1}
            max={8}
            value={s.max_parallel_tasks}
            onChange={(e) =>
              setS({ ...s, max_parallel_tasks: Number(e.target.value) })
            }
          />
        </div>

        <div className="row">
          <label>
            <input
              type="checkbox"
              checked={s.reviewer_enabled}
              onChange={(e) =>
                setS({ ...s, reviewer_enabled: e.target.checked })
              }
              style={{ width: "auto", marginRight: 6 }}
            />
            Run Reviewer pass (one corrective retry on NEEDS_FIX)
          </label>
        </div>

        <div className="row">
          <label>
            <input
              type="checkbox"
              checked={s.cmd_confirm_required}
              onChange={(e) =>
                setS({ ...s, cmd_confirm_required: e.target.checked })
              }
              style={{ width: "auto", marginRight: 6 }}
            />
            Require confirmation for shell commands not in the allow-list
          </label>
        </div>

        {/*
         * OC-Titan §VI.2/§VI.3 — dev-mode toggle. Single checkbox that
         * flips the three opt-in backend gates so the user can see the
         * full self-healing pipeline (dependency guard → auto-install →
         * compiler gate → execution → runtime validation) end-to-end
         * without hand-editing the individual flags. Dependency guard
         * is intentionally untouched: it is default-on and must not be
         * tied to the dev-mode toggle.
         */}
        <div className="row">
          <label
            title={
              devMode
                ? "execution: ON · runtime: ON · autoinstall: ON"
                : "execution: OFF · runtime: OFF · autoinstall: OFF — enable to see the self-healing pipeline end-to-end"
            }
          >
            <input
              type="checkbox"
              checked={devMode}
              onChange={(e) => {
                const v = e.target.checked;
                setDevMode(v);
                setS((cur) => ({
                  ...cur,
                  autoinstall_enabled: v,
                  security_gate_execute_enabled: v,
                  runtime_validation_enabled: v,
                }));
              }}
              style={{ width: "auto", marginRight: 6 }}
            />
            Dev-mode: enable OC-Titan self-healing pipeline (execution +
            runtime + autoinstall)
          </label>
          <div
            style={{
              fontSize: 11,
              color: "var(--muted, #888)",
              marginTop: 2,
              marginLeft: 22,
            }}
          >
            execution: {devMode ? "ON" : "OFF"} · runtime:{" "}
            {devMode ? "ON" : "OFF"} · autoinstall:{" "}
            {devMode ? "ON" : "OFF"} · dependency_guard: default
          </div>
        </div>

        <div className="row">
          <label>
            Auto-approve allow-list (one command prefix per line, e.g. “npm
            run”)
          </label>
          <textarea
            rows={5}
            value={allowListText}
            onChange={(e) => setAllowListText(e.target.value)}
            style={{ fontFamily: "var(--mono)", fontSize: 12 }}
          />
        </div>

        <div className="actions">
          <button onClick={onClose}>Cancel</button>
          <button className="primary" disabled={saving} onClick={() => void save()}>
            {saving ? "Saving…" : "Save"}
          </button>
        </div>
      </div>
    </div>
  );
}

/**
 * Small amber warning chip shown under planner or executor model
 * fields when the chosen model name looks ≤3B. Tone matches the
 * existing yellow "reachable, but model not pulled" chip above so
 * users recognise it as an advisory, not an error. Scenario-A §9.2
 * F-4.
 */
function SmallModelWarning({ role }: { role: "planner" | "executor" }) {
  return (
    <div
      role="note"
      className="settings-small-model-warning"
      aria-label={`${role} model size warning`}
    >
      <span aria-hidden="true" className="sa-icon">
        ⚠
      </span>
      <span>
        Small models (≤3 B) frequently fail to emit tool calls — Scenario A
        reproduced this on <code>llama3.2:1b</code>. Prefer{" "}
        <code>qwen2.5-coder:7b</code> or <code>llama3.1:8b</code> for the{" "}
        {role}.
      </span>
    </div>
  );
}
