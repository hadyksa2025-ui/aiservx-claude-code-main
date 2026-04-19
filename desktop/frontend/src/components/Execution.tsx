import type { ExecutionEvent } from "../types";

function fmtTime(ms: number) {
  const d = new Date(ms);
  return d.toLocaleTimeString();
}

export function Execution({ events }: { events: ExecutionEvent[] }) {
  if (events.length === 0) {
    return (
      <div className="empty-state">
        Tool calls, command output, and file changes will appear here.
      </div>
    );
  }
  return (
    <div className="exec-list">
      {events.map((e, i) => {
        if (e.kind === "tool_call") {
          return (
            <div key={i} className="exec-item call">
              <div className="title">
                {fmtTime(e.at)} → tool_call: <strong>{e.call.name}</strong>
              </div>
              <pre>{JSON.stringify(e.call.args, null, 2)}</pre>
            </div>
          );
        }
        if (e.kind === "tool_result") {
          return (
            <div
              key={i}
              className={"exec-item result " + (e.result.ok ? "ok" : "err")}
            >
              <div className="title">
                {fmtTime(e.at)} ← tool_result {e.result.ok ? "✓" : "✗"} (id {e.result.id.slice(0, 6)})
              </div>
              {e.result.diff ? (
                <pre>{e.result.diff}</pre>
              ) : (
                <pre>{e.result.output || "(empty)"}</pre>
              )}
            </div>
          );
        }
        if (e.kind === "error") {
          return (
            <div key={i} className="exec-item error">
              <div className="title">{fmtTime(e.at)} — error</div>
              <pre>{e.text}</pre>
            </div>
          );
        }
        return (
          <div key={i} className="exec-item info">
            <div className="title">{fmtTime(e.at)} — info</div>
            <pre>{e.text}</pre>
          </div>
        );
      })}
    </div>
  );
}
