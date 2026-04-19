import { useEffect, useState } from "react";
import { api } from "../api";
import type { Settings } from "../types";

const DEFAULTS: Settings = {
  openrouter_api_key: "",
  openrouter_model: "openrouter/auto",
  ollama_base_url: "http://localhost:11434",
  ollama_model: "llama3.1:8b",
  reviewer_enabled: true,
  max_iterations: 8,
  cmd_confirm_required: true,
  cmd_allow_list: [],
  autonomous_mode: false,
  max_retries_per_task: 3,
  max_total_tasks: 20,
};

export function SettingsModal({ onClose }: { onClose: () => void }) {
  const [s, setS] = useState<Settings>(DEFAULTS);
  const [loaded, setLoaded] = useState(false);
  const [saving, setSaving] = useState(false);
  const [allowListText, setAllowListText] = useState("");

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
      const normalized: Settings = {
        ...s,
        max_iterations: Math.max(1, Math.min(16, Number(s.max_iterations) || 8)),
        max_retries_per_task: Math.max(
          0,
          Math.min(10, Number(s.max_retries_per_task) || 3),
        ),
        max_total_tasks: Math.max(
          1,
          Math.min(100, Number(s.max_total_tasks) || 20),
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
