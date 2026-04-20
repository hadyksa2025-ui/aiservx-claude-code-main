import { useEffect, useState } from "react";
import { api } from "../api";
import type { Settings } from "../types";

type ModelPreset = {
  label: string;
  value: string;
  tone: "openai" | "anthropic" | "google" | "meta" | "ollama" | "default";
};

type OpenRouterPresetCandidate = {
  label: string;
  tone: Extract<ModelPreset["tone"], "openai" | "anthropic" | "google" | "meta">;
  candidates: string[];
  fallback: RegExp[];
};

const OPENROUTER_PRESET_CANDIDATES: OpenRouterPresetCandidate[] = [
  {
    label: "GPT-4o",
    tone: "openai",
    candidates: [
      "openai/gpt-4o",
      "openai/gpt-4.1",
      "openai/gpt-4-turbo",
      "openai/gpt-4",
    ],
    fallback: [/^openai\/gpt-4o/i, /^openai\/gpt-4\./i, /^openai\/gpt-4/i],
  },
  {
    label: "Claude Sonnet",
    tone: "anthropic",
    candidates: [
      "anthropic/claude-3.5-sonnet",
      "anthropic/claude-3.7-sonnet",
      "anthropic/claude-3-opus",
      "anthropic/claude-3-sonnet",
    ],
    fallback: [/^anthropic\/.*sonnet/i, /^anthropic\/claude-3/i],
  },
  {
    label: "Gemini",
    tone: "google",
    candidates: [
      "google/gemini-2.0-flash-exp",
      "google/gemini-2.0-flash",
      "google/gemini-1.5-pro",
      "google/gemini-1.5-flash",
    ],
    fallback: [/^google\/gemini-2\./i, /^google\/gemini/i],
  },
  {
    label: "Llama",
    tone: "meta",
    candidates: [
      "meta-llama/llama-4-scout-17b-16e-instruct",
      "meta-llama/llama-4-maverick-17b-128e-instruct",
      "meta-llama/llama-3.1-70b-instruct",
      "meta-llama/llama-3.1-405b-instruct",
    ],
    fallback: [/^meta-llama\/llama-4/i, /^meta-llama\/llama/i],
  },
];

const OPENROUTER_PRESETS_FALLBACK: ModelPreset[] = [
  { label: "GPT-4o", value: "openai/gpt-4o", tone: "openai" },
  { label: "Claude Sonnet", value: "anthropic/claude-3.5-sonnet", tone: "anthropic" },
  { label: "Gemini", value: "google/gemini-2.0-flash-exp", tone: "google" },
  {
    label: "Llama",
    value: "meta-llama/llama-4-scout-17b-16e-instruct",
    tone: "meta",
  },
];

const OLLAMA_PRESETS: ModelPreset[] = [
  { label: "Qwen Coder", value: "qwen2.5-coder:7b", tone: "ollama" },
  { label: "DeepSeek Coder", value: "deepseek-coder:6.7b", tone: "ollama" },
  { label: "Llama 3.1", value: "llama3.1:8b", tone: "ollama" },
  { label: "Mistral", value: "mistral:7b", tone: "ollama" },
];

function PresetButtons({
  value,
  presets,
  onPick,
  defaultLabel,
  defaultValue,
}: {
  value: string;
  presets: ModelPreset[];
  onPick: (next: string) => void;
  defaultLabel?: string;
  defaultValue?: string;
}) {
  return (
    <div className="preset-row" role="group" aria-label="Model presets">
      {defaultValue !== undefined && (
        <button
          type="button"
          className={`preset-btn ai-pill tone-default ${value === defaultValue ? "is-active" : ""}`}
          onClick={() => onPick(defaultValue)}
          title={defaultValue === "" ? "Use provider default" : defaultValue}
        >
          {defaultLabel ?? "Default"}
        </button>
      )}
      {presets.map((p) => (
        <button
          key={p.value}
          type="button"
          className={`preset-btn ai-pill tone-${p.tone} ${value === p.value ? "is-active" : ""}`}
          onClick={() => onPick(p.value)}
          title={p.value}
        >
          {p.label}
        </button>
      ))}
    </div>
  );
}

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

export function SettingsModal({ onClose }: { onClose: () => void }) {
  const [s, setS] = useState<Settings>(DEFAULTS);
  const [loaded, setLoaded] = useState(false);
  const [saving, setSaving] = useState(false);
  const [allowListText, setAllowListText] = useState("");
  const [probe, setProbe] = useState<ProbeState>({ kind: "idle" });
  const [orProbe, setOrProbe] = useState<OpenRouterProbeState>({
    kind: "idle",
  });
  const [openRouterCatalog, setOpenRouterCatalog] = useState<string[] | null>(
    null,
  );

  const resolveOpenRouterPresets = (catalog: string[] | null): ModelPreset[] => {
    if (!catalog || catalog.length === 0) return OPENROUTER_PRESETS_FALLBACK;
    const have = new Set(catalog);

    const findRegexMatch = (patterns: RegExp[]): string | undefined => {
      for (const re of patterns) {
        const hit = catalog.find((id) => re.test(id));
        if (hit) return hit;
      }
      return undefined;
    };

    const resolved: ModelPreset[] = [];
    for (const p of OPENROUTER_PRESET_CANDIDATES) {
      const pick = p.candidates.find((c) => have.has(c));
      const best = pick ?? findRegexMatch(p.fallback);
      if (best) resolved.push({ label: p.label, value: best, tone: p.tone });
    }
    return resolved.length > 0 ? resolved : OPENROUTER_PRESETS_FALLBACK;
  };

  const openrouterPresets = resolveOpenRouterPresets(openRouterCatalog);

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
        setS(merged);
        setAllowListText(merged.cmd_allow_list.join("\n"));
      } catch {
        /* use defaults */
      } finally {
        setLoaded(true);
      }
    })();
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
        <h2>Settings</h2>

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
          <label>OpenRouter API key (required for cloud / hybrid modes)</label>
          <input
            type="password"
            placeholder="sk-or-…"
            value={s.openrouter_api_key}
            onChange={(e) => setS({ ...s, openrouter_api_key: e.target.value })}
          />
        </div>
        <div className="row">
          <label>OpenRouter default model</label>
          <input
            value={s.openrouter_model}
            onChange={(e) => setS({ ...s, openrouter_model: e.target.value })}
          />
          <PresetButtons
            value={s.openrouter_model}
            presets={openrouterPresets}
            defaultLabel="Auto"
            defaultValue={DEFAULTS.openrouter_model}
            onPick={(next) => setS({ ...s, openrouter_model: next })}
          />
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
            <div className="preset-input">
              <input
                placeholder="e.g. anthropic/claude-3.5-sonnet"
                value={s.planner_model}
                onChange={(e) => setS({ ...s, planner_model: e.target.value })}
              />
              <PresetButtons
                value={s.planner_model}
                presets={openrouterPresets}
                defaultLabel="Default"
                defaultValue=""
                onPick={(next) => setS({ ...s, planner_model: next })}
              />
            </div>
            {modelLooksSmall(s.planner_model) && (
              <>
                <span />
                <SmallModelWarning role="planner" />
              </>
            )}
            <span style={{ fontSize: 12, color: "#bbb" }}>Executor</span>
            <div className="preset-input">
              <input
                placeholder="e.g. deepseek-coder:6.7b"
                value={s.executor_model}
                onChange={(e) => setS({ ...s, executor_model: e.target.value })}
              />
              <PresetButtons
                value={s.executor_model}
                presets={OLLAMA_PRESETS}
                defaultLabel="Default"
                defaultValue=""
                onPick={(next) => setS({ ...s, executor_model: next })}
              />
            </div>
            {modelLooksSmall(s.executor_model) && (
              <>
                <span />
                <SmallModelWarning role="executor" />
              </>
            )}
            <span style={{ fontSize: 12, color: "#bbb" }}>Reviewer</span>
            <div className="preset-input">
              <input
                placeholder="e.g. anthropic/claude-3.5-sonnet"
                value={s.reviewer_model}
                onChange={(e) => setS({ ...s, reviewer_model: e.target.value })}
              />
              <PresetButtons
                value={s.reviewer_model}
                presets={openrouterPresets}
                defaultLabel="Default"
                defaultValue=""
                onPick={(next) => setS({ ...s, reviewer_model: next })}
              />
            </div>
          </div>
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
          <PresetButtons
            value={s.ollama_model}
            presets={OLLAMA_PRESETS}
            defaultLabel="Default"
            defaultValue={DEFAULTS.ollama_model}
            onPick={(next) => setS({ ...s, ollama_model: next })}
          />
          {modelLooksSmall(s.ollama_model) && (
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
              {probe.kind === "testing" ? "Testing…" : "Test Ollama connection"}
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
