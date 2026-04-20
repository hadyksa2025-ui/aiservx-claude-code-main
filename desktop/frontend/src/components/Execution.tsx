import { useMemo, useRef } from "react";
import { List, useDynamicRowHeight, type RowComponentProps } from "react-window";
import type { AgentRole, ExecutionEvent, StepEvent } from "../types";

function fmtTime(ms: number) {
  const d = new Date(ms);
  return d.toLocaleTimeString();
}

function roleBadge(role?: AgentRole) {
  if (!role) return null;
  return <span className={"role-chip chip-" + role}>{role}</span>;
}

function DiffBlock({ diff }: { diff: string }) {
  const lines = diff.split("\n");
  return (
    <pre className="diff">
      {lines.map((line, i) => {
        let cls = "diff-ctx";
        if (line.startsWith("+++") || line.startsWith("---")) cls = "diff-meta";
        else if (line.startsWith("@@")) cls = "diff-hunk";
        else if (line.startsWith("+")) cls = "diff-add";
        else if (line.startsWith("-")) cls = "diff-del";
        return (
          <span key={i} className={cls}>
            {line}
            {"\n"}
          </span>
        );
      })}
    </pre>
  );
}

function StepRow({ step }: { step: StepEvent }) {
  const status =
    step.status === "running" ? "⋯" : step.status === "done" ? "✓" : "✗";
  return (
    <div className={"step-row status-" + step.status}>
      <span className="step-status" aria-hidden>
        {status}
      </span>
      {roleBadge(step.role)}
      <span className="step-title">{step.title}</span>
    </div>
  );
}

/**
 * Rendered event list rows. Steps are collapsed into the agent
 * timeline (above the virtualised list) so we never re-render a
 * running step as its state changes — the timeline owns that.
 */
type Row = { kind: "inline"; event: ExecutionEvent; origIndex: number };

function EventRow({ event }: { event: ExecutionEvent }) {
  if (event.kind === "tool_call") {
    return (
      <div className="exec-item call">
        <div className="title">
          {fmtTime(event.at)} → tool_call: <strong>{event.call.name}</strong>
          {roleBadge(event.call.role)}
        </div>
        <pre>{JSON.stringify(event.call.args, null, 2)}</pre>
      </div>
    );
  }
  if (event.kind === "tool_result") {
    return (
      <div
        className={"exec-item result " + (event.result.ok ? "ok" : "err")}
      >
        <div className="title">
          {fmtTime(event.at)} ← tool_result {event.result.ok ? "✓" : "✗"} (id{" "}
          {event.result.id.slice(0, 6)}){roleBadge(event.result.role)}
        </div>
        {event.result.diff ? (
          <DiffBlock diff={event.result.diff} />
        ) : (
          <pre>{event.result.output || "(empty)"}</pre>
        )}
      </div>
    );
  }
  if (event.kind === "error") {
    return (
      <div className="exec-item error">
        <div className="title">
          {fmtTime(event.at)} — error{roleBadge(event.role)}
        </div>
        <pre>{event.text}</pre>
      </div>
    );
  }
  if (event.kind === "info") {
    return (
      <div className="exec-item info">
        <div className="title">{fmtTime(event.at)} — info</div>
        <pre>{event.text}</pre>
      </div>
    );
  }
  // "step" events are rendered in the timeline above; defensive null
  // keeps the exhaustiveness check happy without silently producing
  // an empty row in the virtual list.
  return null;
}

/**
 * react-window row component. The row element is absolutely
 * positioned inside the virtual scroll container, so our content
 * wrapper absorbs its own vertical margin inside `padding` to avoid
 * margin-collapse weirdness with the measurer.
 */
function VirtualRow({
  index,
  style,
  ariaAttributes,
  rows,
}: RowComponentProps<{ rows: Row[] }>) {
  const row = rows[index];
  if (!row) return null;
  return (
    <div style={style} {...ariaAttributes} className="exec-row-wrap">
      <EventRow event={row.event} />
    </div>
  );
}

/**
 * Event-log panel. The inline event rows (tool_call / tool_result /
 * error / info) are rendered through a virtualised `react-window`
 * `List` (audit §7.6) so long autonomous runs stay scroll-jank-free
 * even at the 500-entry ring-buffer cap. Step events are deliberately
 * lifted out and rendered as a compact, always-visible timeline above
 * the virtual list — that timeline is short (one entry per plan step)
 * and benefits from being present-even-while-scrolling.
 */
export function Execution({ events }: { events: ExecutionEvent[] }) {
  const steps = useMemo(() => {
    const byIndex = new Map<number, StepEvent>();
    for (const e of events) {
      if (e.kind === "step") byIndex.set(e.step.index, e.step);
    }
    return [...byIndex.values()].sort((a, b) => a.index - b.index);
  }, [events]);

  const rows = useMemo<Row[]>(() => {
    const out: Row[] = [];
    for (let i = 0; i < events.length; i++) {
      const e = events[i];
      if (e.kind === "step") continue; // shown in the timeline above
      out.push({ kind: "inline", event: e, origIndex: i });
    }
    return out;
  }, [events]);

  // Virtualisation uses dynamic row heights (tool outputs and diffs
  // vary wildly) measured via ResizeObserver. `key` bumps when the
  // total event count shifts by more than the ring-buffer trim so
  // cached heights stay coherent after `clear`.
  const heightKey = rows.length > 0 ? rows[0].origIndex : 0;
  const rowHeight = useDynamicRowHeight({
    defaultRowHeight: 48,
    key: heightKey,
  });

  const listContainerRef = useRef<HTMLDivElement>(null);

  if (events.length === 0) {
    return (
      <div className="empty-state">
        Tool calls, command output, and file changes will appear here.
      </div>
    );
  }

  return (
    <div className="exec-list" ref={listContainerRef}>
      {steps.length > 0 && (
        <div className="step-timeline">
          <div className="step-timeline-title">agent timeline</div>
          {steps.map((s) => (
            <StepRow key={s.index} step={s} />
          ))}
        </div>
      )}
      {rows.length > 0 && (
        <div className="exec-virtual-wrap">
          <List
            rowCount={rows.length}
            rowHeight={rowHeight}
            rowComponent={VirtualRow}
            rowProps={{ rows }}
            overscanCount={4}
            className="exec-virtual-list"
          />
        </div>
      )}
    </div>
  );
}
