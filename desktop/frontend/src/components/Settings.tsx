import { useEffect, useState } from "react";
import { api } from "../api";
import type { Settings } from "../types";

const DEFAULTS: Settings = {
  openrouter_api_key: "",
  openrouter_model: "openrouter/auto",
  ollama_base_url: "http://localhost:11434",
  ollama_model: "llama3.1:8b",
};

export function SettingsModal({ onClose }: { onClose: () => void }) {
  const [s, setS] = useState<Settings>(DEFAULTS);
  const [loaded, setLoaded] = useState(false);
  const [saving, setSaving] = useState(false);

  useEffect(() => {
    void (async () => {
      try {
        const cur = await api.getSettings();
        setS({ ...DEFAULTS, ...cur });
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
      await api.saveSettings(s);
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
