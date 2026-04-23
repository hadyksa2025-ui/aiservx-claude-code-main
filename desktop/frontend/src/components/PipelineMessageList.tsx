import { useMemo } from "react";
import { useAppStore } from "../store";
import { SystemAction, type SystemActionTone } from "./SystemAction";
import type { PipelineRole, PipelineStepEvent } from "../types";

/**
 * OC-Titan §VI.2 / §VI.3 — pipeline-event message list.
 *
 * A strictly thin renderer over `pipelineEvents` from the store. The
 * three message tiers are:
 *
 *  - **Tier 2 (ThinkingBlock-style)** — planning / guard / compiler /
 *    security. These are the "what did the model / static checker
 *    decide" rows, rendered as left-aligned block rows with a role
 *    chip.
 *  - **Tier 3 (SystemAction pill)** — autoinstall / execution /
 *    runtime. These are inline micro-annotations for system actions
 *    that were actually performed (or attempted). Rendered via the
 *    existing {@link SystemAction} component so they share palette
 *    with tool-call pills elsewhere in the chat.
 *  - **Tier 1 (FinalAnswer)** — the terminal assistant bubble. This
 *    is *not* rendered here — it is authored by the executor and
 *    already surfaces through `Chat.tsx`'s existing bubble pipeline.
 *    The pipeline slice only drives the 6-state phase chip and the
 *    intermediate system-action / thinking rows.
 *
 * No business logic lives here: every row is a direct read from the
 * event payload. All state transitions (running / waiting_confirm /
 * retrying / completed / failed) are derived by `nextPipelinePhase()`
 * in `store.ts` and consumed via `pipelinePhase` elsewhere — this
 * component does *not* interpret them.
 */

/**
 * Tier-2 roles render as a left-aligned block row. Everything else is
 * a Tier-3 inline pill. Order matters (it defines how we pick the
 * block-vs-pill variant at render time).
 */
const TIER2_ROLES: ReadonlySet<PipelineRole> = new Set<PipelineRole>([
  "guard",
  "compiler",
  "security",
]);

function roleLabel(role: PipelineRole): string {
  switch (role) {
    case "guard":
      return "Dependency guard";
    case "compiler":
      return "Compiler gate";
    case "security":
      return "Security classifier";
    case "autoinstall":
      return "Auto-install";
    case "execution":
      return "Execution";
    case "runtime":
      return "Runtime validation";
  }
}

/** Map a pipeline status to a SystemAction tone. */
function toneForStatus(
  status: PipelineStepEvent["status"],
): SystemActionTone {
  switch (status) {
    case "running":
      return "info";
    case "done":
      return "success";
    case "warning":
      return "warn";
    case "failed":
      return "error";
    default:
      return "info";
  }
}

/** Short glyph that matches the status, for the pill icon column. */
function iconForStatus(status: PipelineStepEvent["status"]): string {
  switch (status) {
    case "running":
      return "⏵";
    case "done":
      return "✓";
    case "warning":
      return "!";
    case "failed":
      return "✗";
    default:
      return "·";
  }
}

/**
 * Build a compact human-readable suffix for the event label: attempt
 * number, exit code, optional missing-deps list, optional security
 * classification. Used by both tiers.
 */
function describeEvent(e: PipelineStepEvent): string {
  const parts: string[] = [];
  if (typeof e.attempt === "number" && e.attempt > 0) {
    parts.push(`attempt ${e.attempt}`);
  }
  if (typeof e.exit_code === "number") {
    parts.push(`exit ${e.exit_code}`);
  }
  if (e.class && typeof e.class === "string") {
    parts.push(e.class);
  }
  if (Array.isArray(e.missing) && e.missing.length > 0) {
    const preview = e.missing.slice(0, 3).join(", ");
    const more = e.missing.length > 3 ? ` +${e.missing.length - 3}` : "";
    parts.push(`missing: ${preview}${more}`);
  }
  if (typeof e.reason === "string" && e.reason.length > 0) {
    parts.push(e.reason);
  }
  return parts.join(" · ");
}

export function PipelineMessageList() {
  const events = useAppStore((s) => s.pipelineEvents);

  // Derive stable keys up-front so React reconciliation doesn't move
  // rows around when a new event lands at the tail. We key on the
  // position within the ring-buffered event list: the store itself
  // drops oldest events first, so `${i}-${role}-${label}` is stable
  // for as long as a given event is visible.
  const rows = useMemo(() => {
    return events.map((e, i) => ({
      key: `${i}-${e.role}-${e.label}`,
      event: e,
    }));
  }, [events]);

  if (rows.length === 0) return null;

  return (
    <div
      className="pipeline-message-list"
      role="log"
      aria-live="polite"
      aria-label="OC-Titan pipeline events"
    >
      {rows.map(({ key, event }) => {
        const isTier2 = TIER2_ROLES.has(event.role);
        const suffix = describeEvent(event);
        const text = suffix
          ? `${event.label} — ${suffix}`
          : event.label;
        if (isTier2) {
          // Tier-2 "thinking" row: block-level, left-aligned, with a
          // role chip and the event label. Collapsed styling matches
          // the existing ThinkingBlock header (styles.css ships
          // shared palette).
          return (
            <div
              key={key}
              className={`pipeline-row pipeline-tier-2 pipeline-role-${event.role} pipeline-status-${event.status}`}
            >
              <span className={`pipeline-chip pipeline-chip-${event.role}`}>
                {roleLabel(event.role)}
              </span>
              <span
                className={`pipeline-status pipeline-status-chip-${event.status}`}
                aria-label={`status ${event.status}`}
              >
                {iconForStatus(event.status)}
              </span>
              <span className="pipeline-text">{text}</span>
            </div>
          );
        }
        // Tier-3 inline system-action pill: reuses the existing
        // SystemAction palette so autoinstall / execution / runtime
        // micro-events sit visually next to the chat's tool-call
        // pills.
        return (
          <div key={key} className="pipeline-row pipeline-tier-3">
            <span className="pipeline-role-prefix">
              {roleLabel(event.role)}:
            </span>
            <SystemAction
              icon={iconForStatus(event.status)}
              text={text}
              tone={toneForStatus(event.status)}
              title={`${event.role} · ${event.label} · ${event.status}`}
            />
          </div>
        );
      })}
    </div>
  );
}
