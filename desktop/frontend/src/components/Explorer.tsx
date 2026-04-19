import { useCallback, useEffect, useMemo, useState } from "react";
import { api } from "../api";
import type { FsEntry } from "../types";

type Node = FsEntry & { children?: FsEntry[]; expanded?: boolean };

export function Explorer({ projectDir }: { projectDir: string }) {
  const [root, setRoot] = useState<Node[]>([]);
  const [expanded, setExpanded] = useState<Record<string, FsEntry[] | undefined>>({});
  const [selected, setSelected] = useState<string | null>(null);
  const [preview, setPreview] = useState<{ path: string; content: string } | null>(null);
  const [error, setError] = useState<string | null>(null);

  const loadDir = useCallback(
    async (subPath: string) => {
      try {
        const entries = await api.listDir(projectDir, subPath);
        entries.sort((a, b) => {
          if (a.is_dir !== b.is_dir) return a.is_dir ? -1 : 1;
          return a.name.localeCompare(b.name);
        });
        return entries;
      } catch (e) {
        setError(String(e));
        return [];
      }
    },
    [projectDir],
  );

  useEffect(() => {
    void (async () => {
      const entries = await loadDir("");
      setRoot(entries);
      setExpanded({ "": entries });
    })();
  }, [loadDir]);

  const toggle = useCallback(
    async (entry: FsEntry) => {
      if (!entry.is_dir) {
        setSelected(entry.path);
        try {
          const content = await api.readFile(projectDir, entry.path);
          const clipped = content.length > 20000 ? content.slice(0, 20000) + "\n…" : content;
          setPreview({ path: entry.path, content: clipped });
        } catch (e) {
          setPreview({ path: entry.path, content: `Error: ${String(e)}` });
        }
        return;
      }
      if (expanded[entry.path]) {
        const next = { ...expanded };
        delete next[entry.path];
        setExpanded(next);
      } else {
        const entries = await loadDir(entry.path);
        setExpanded({ ...expanded, [entry.path]: entries });
      }
    },
    [expanded, loadDir, projectDir],
  );

  const tree = useMemo(() => {
    const render = (entries: FsEntry[], depth: number): JSX.Element[] => {
      const out: JSX.Element[] = [];
      for (const e of entries) {
        const isOpen = !!expanded[e.path];
        out.push(
          <div
            key={e.path}
            className={"fs-node" + (selected === e.path ? " is-selected" : "")}
            style={{ paddingLeft: 6 + depth * 10 }}
            onClick={() => void toggle(e)}
            title={e.path}
          >
            <span className="caret">{e.is_dir ? (isOpen ? "▾" : "▸") : " "}</span>
            <span className="icon">{e.is_dir ? "📁" : "📄"}</span>
            <span>{e.name}</span>
          </div>,
        );
        if (e.is_dir && isOpen) {
          const children = expanded[e.path] ?? [];
          out.push(
            <div key={e.path + "::children"} className="fs-children">
              {render(children, depth + 1)}
            </div>,
          );
        }
      }
      return out;
    };
    return render(root, 0);
  }, [root, expanded, selected, toggle]);

  return (
    <>
      {error && <div className="empty-state" style={{ color: "var(--danger)" }}>{error}</div>}
      <div>{tree}</div>
      {preview && (
        <div className="file-preview">
          <div style={{ marginBottom: 6, color: "var(--fg-dim)" }}>
            {preview.path}
          </div>
          <div>{preview.content}</div>
        </div>
      )}
    </>
  );
}
