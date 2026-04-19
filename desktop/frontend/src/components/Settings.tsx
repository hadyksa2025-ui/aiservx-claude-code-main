import { useEffect, useState } from "react";
import { api } from "../api";
import type { Settings } from "../types";

const DEFAULTS: Settings = {
  openrouter_api_key: "",
  openrouter_model: "openrouter/auto",
  ollama_base_url: "http://localhost:11434",
  ollama_model: "deepseek-coder:6.7b",
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

export function SettingsModal({ onClose }: { onClose: () => void }) {
  const [s, setS] = useState<Settings>(DEFAULTS);
  const [loaded, setLoaded] = useState(false);
  const [saving, setSaving] = useState(false);
  const [allowListText, setAllowListText] = useState("");
  const [probe, setProbe] = useState<ProbeState>({ kind: "idle" });

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

  // Reset the probe result whenever the tested inputs change. Without this
  // the UI would render a stale green/yellow/red status against the *current*
  // form value for ollama_model/ollama_base_url, even though the probe was
  // computed against the previous values — e.g. showing "✓ model foo
  // available" after the user has just typed a completely different model.
  useEffect(() => {
    setProbe({ kind: "idle" });
  }, [s.ollama_base_url, s.ollama_model]);

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
          <label>OpenRouter API key (optional — enables the planner)</label>
          <input
            type="password"
            placeholder="sk-or-…"
            value={s.openrouter_api_key}
            onChange={(e) => setS({ ...s, openrouter_api_key: e.target.value })}
          />
        </div>
        <div className="row">
          <label>OpenRouter model</label>
          <input
            value={s.openrouter_model}
            onChange={(e) => setS({ ...s, openrouter_model: e.target.value })}
          />
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
