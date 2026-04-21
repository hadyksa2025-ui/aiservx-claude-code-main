import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import {
  api,
  onEvent,
  type DoneEvent,
  type ExecutorUnparsedEvent,
  type TokenEvent,
  type ErrorEvent,
} from "../api";
import type { AgentRole, ChatMessage, StepEvent } from "../types";
import { useAppStore } from "../store";
import { FinalAnswerBubble } from "./FinalAnswerBubble";
import { SystemAction } from "./SystemAction";
import { ThinkingBlock } from "./ThinkingBlock";

type Props = {
  projectDir: string | null;
  disabled: boolean;
};

function uid() {
  return Math.random().toString(36).slice(2, 10);
}

/**
 * Classifies each message into one of four rendering tiers.
 *
 * Tier definitions (from the audit):
 *  - `user`     — the user's prompt (rendered as a user bubble).
 *  - `system`   — a synthetic system message (renders as a
 *                 {@link SystemAction} pill; never the final answer).
 *  - `final`    — the last non-reviewer assistant bubble in a turn;
 *                 the executor's actual answer. Rendered via
 *                 {@link FinalAnswerBubble}.
 *  - `thinking` — everything else authored by an agent (planner
 *                 output, earlier executor iterations before a reviewer
 *                 retry, reviewer verdicts). Rendered via
 *                 {@link ThinkingBlock}.
 *
 * A "turn" starts at a user message and ends at the next one. Within a
 * turn the final-answer bubble is the **last** assistant message whose
 * `streaming_role` is `executor` (or missing, i.e. legacy / synthesised
 * bubbles). Planner and reviewer bubbles are never the final answer —
 * even when they're the last assistant in a turn, and even during
 * streaming. Scenario-A §9.2 F-5: without explicitly excluding the
 * planner, the first streaming planner bubble renders as `FinalAnswer`
 * until a later executor bubble arrives and demotes it, which is
 * visually jarring.
 */
type Tier = "user" | "system" | "final" | "thinking";

function classifyMessages(messages: ChatMessage[]): Tier[] {
  const tiers: Tier[] = new Array(messages.length).fill("thinking");
  // Track the last-final candidate per turn. We walk forward and reset
  // the candidate whenever a new user message opens a turn.
  let turnFinalIdx = -1;
  for (let i = 0; i < messages.length; i++) {
    const m = messages[i];
    if (m.role === "user") {
      if (turnFinalIdx >= 0) tiers[turnFinalIdx] = "final";
      turnFinalIdx = -1;
      tiers[i] = "user";
      continue;
    }
    if (m.role === "system" || m.role === "tool") {
      tiers[i] = "system";
      continue;
    }
    // assistant
    const r = m.streaming_role;
    if (r === "planner" || r === "reviewer") {
      // Planner and reviewer are always thinking; they never promote
      // to the turn's final-answer slot. See doc comment above.
      tiers[i] = "thinking";
      continue;
    }
    // executor (or legacy `undefined` role) is a candidate for "final".
    turnFinalIdx = i;
    tiers[i] = "thinking";
  }
  if (turnFinalIdx >= 0) tiers[turnFinalIdx] = "final";
  return tiers;
}

export function Chat({ projectDir, disabled }: Props) {
  // Messages live in the global store so other components (e.g. a
  // future task-panel that needs to append a system note) can push
  // without another prop round-trip. Audit §7.6.
  const messages = useAppStore((s) => s.messages);
  const setMessages = useAppStore((s) => s.setMessages);
  const setSettingsOpen = useAppStore((s) => s.setSettingsOpen);
  const [input, setInput] = useState("");
  const [sending, setSending] = useState(false);
  const scrollRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    scrollRef.current?.scrollTo({ top: scrollRef.current.scrollHeight });
  }, [messages]);

  // Stream tokens into the chat. Each agent-role run gets its own bubble so
  // the user can see Planner → Executor → Reviewer unfold in order.
  useEffect(() => {
    const unlistens: Array<Promise<() => void>> = [];
    unlistens.push(
      onEvent<TokenEvent>("ai:token", (p) => {
        setMessages((prev) => {
          const last = prev[prev.length - 1];
          if (last && last.streaming && last.streaming_role === p.role) {
            return [
              ...prev.slice(0, -1),
              { ...last, content: last.content + p.text },
            ];
          }
          return [
            ...prev,
            {
              id: uid(),
              role: "assistant",
              content: p.text,
              streaming: true,
              streaming_role: p.role,
              started_at: Date.now(),
            },
          ];
        });
      }),
    );
    unlistens.push(
      onEvent<StepEvent>("ai:step", (p) => {
        setMessages((prev) => {
          // Attach provider / model / duration to the most recent
          // streaming bubble for this role. Done steps also freeze
          // `ended_at` so the ThinkingBlock can show a duration even
          // before the global `ai:done` arrives.
          const now = Date.now();
          for (let i = prev.length - 1; i >= 0; i--) {
            const m = prev[i];
            if (
              m.role === "assistant" &&
              m.streaming_role === p.role &&
              (m.streaming || m.ended_at == null)
            ) {
              const next = [...prev];
              next[i] = {
                ...m,
                provider: p.provider ?? m.provider,
                model: p.model ?? m.model,
                ended_at: p.status !== "running" ? now : m.ended_at,
              };
              return next;
            }
          }
          return prev;
        });
      }),
    );
    unlistens.push(
      onEvent<DoneEvent>("ai:done", () => {
        const now = Date.now();
        setMessages((prev) =>
          prev.map((m) =>
            m.streaming
              ? { ...m, streaming: false, ended_at: m.ended_at ?? now }
              : m,
          ),
        );
      }),
    );
    unlistens.push(
      onEvent<ErrorEvent>("ai:error", (p) => {
        setMessages((prev) => [
          ...prev,
          {
            id: uid(),
            role: "system",
            content: `[${p.role ?? "ai"}] error: ${p.message}`,
          },
        ]);
      }),
    );
    // Scenario-A §9.2 F-4: the backend now emits a dedicated event
    // whenever the executor finishes a turn without producing any
    // parseable tool call (either iteration-0 empty output, or the
    // reviewer couldn't verdict). Promote this to a visible chat
    // bubble — the old behaviour left "review skipped (unparsed)"
    // as a tiny label in the Debug pane nobody noticed. Dedupe by
    // id in case both signals fire in the same turn.
    unlistens.push(
      onEvent<ExecutorUnparsedEvent>("ai:executor_unparsed", (p) => {
        setMessages((prev) => {
          // Scope the dedup to the *current turn* only. A turn
          // starts at the last user message and ends at the next
          // one (see the docstring above `classifyMessages`).
          //
          //   * Scanning the whole list is too broad (PR #13 Devin
          //     Review): once a warn_action lands in turn N, every
          //     future `ai:executor_unparsed` in turns N+1, N+2, …
          //     would be silently suppressed, because `messages` is
          //     only cleared on project switch (App.tsx
          //     `resetMessages` in `openProject`).
          //
          //   * Checking only the last message is too narrow
          //     (PR #12 Devin Review): the backend can fire this
          //     event twice inside one turn (executor iteration-0
          //     empty tool_calls, then reviewer Unknown verdict)
          //     with a reviewer token bubble streaming between the
          //     two emissions, so the real warn_action isn't the
          //     last message any more.
          //
          // Scoping to "everything after the last user message"
          // handles both.
          let turnStart = 0;
          for (let i = prev.length - 1; i >= 0; i--) {
            if (prev[i].role === "user") {
              turnStart = i;
              break;
            }
          }
          const alreadyWarnedThisTurn = prev
            .slice(turnStart)
            .some((m) => m.kind === "warn_action" && m.role === "system");
          if (alreadyWarnedThisTurn) {
            return prev;
          }
          const modelNote = p.model ? ` (executor: \`${p.model}\`)` : "";
          return [
            ...prev,
            {
              id: uid(),
              role: "system",
              content: `Executor output could not be parsed as tool calls${modelNote}. Try a larger executor model (≥7 B) such as \`qwen2.5-coder:7b\` or \`llama3.1:8b\`.`,
              kind: "warn_action",
            },
          ];
        });
      }),
    );
    return () => {
      for (const p of unlistens) void p.then((fn) => fn());
    };
  }, [setMessages]);

  const send = useCallback(async () => {
    if (!projectDir || sending || input.trim().length === 0) return;
    const userMsg: ChatMessage = {
      id: uid(),
      role: "user",
      content: input.trim(),
    };
    const historyForCall = messages.filter((m) => !m.streaming);
    setMessages([...messages, userMsg]);
    setInput("");
    setSending(true);
    try {
      const resp = await api.sendChat(projectDir, userMsg.content, historyForCall);
      setMessages((prev) => {
        const now = Date.now();
        // Clear any lingering streaming flags from dropped frames.
        const cleared = prev.map((m) =>
          m.streaming
            ? { ...m, streaming: false, ended_at: m.ended_at ?? now }
            : m,
        );
        const hasExecutorBubble = cleared.some(
          (m) =>
            m.role === "assistant" &&
            (m.streaming_role === "executor" || !m.streaming_role) &&
            m.content.length > 0,
        );
        if (!hasExecutorBubble && resp.assistant) {
          // Synthesise a final executor bubble when streaming produced
          // no text — otherwise the user would see thinking blocks but
          // no actual answer.
          return [
            ...cleared,
            {
              id: uid(),
              role: "assistant",
              content: resp.assistant,
              streaming_role: "executor",
              tool_calls: resp.tool_calls,
              tool_results: resp.tool_results,
              ended_at: now,
            },
          ];
        }
        // Otherwise attach tool metadata to the last executor bubble so
        // the final-answer tile can show its action count.
        const lastExecutorIdx = [...cleared]
          .map((m, i) => ({ m, i }))
          .reverse()
          .find(
            ({ m }) =>
              m.role === "assistant" &&
              (m.streaming_role === "executor" || !m.streaming_role),
          )?.i;
        if (lastExecutorIdx != null) {
          const next = [...cleared];
          next[lastExecutorIdx] = {
            ...next[lastExecutorIdx],
            tool_calls: resp.tool_calls,
            tool_results: resp.tool_results,
          };
          return next;
        }
        return cleared;
      });
    } catch (e) {
      setMessages((prev) => [
        ...prev.map((m) =>
          m.streaming ? { ...m, streaming: false } : m,
        ),
        {
          id: uid(),
          role: "assistant",
          content: `Error: ${String(e)}`,
        },
      ]);
    } finally {
      setSending(false);
    }
  }, [input, messages, projectDir, sending, setMessages]);

  const tiers = useMemo(() => classifyMessages(messages), [messages]);

  return (
    <div className="chat">
      <div className="chat-messages" ref={scrollRef}>
        {messages.length === 0 && (
          <div className="empty-state">
            Ask the AI to read, edit, or run commands in your project.
          </div>
        )}
        {messages.map((m, i) =>
          renderMessage(m, tiers[i], () => setSettingsOpen(true)),
        )}
        {sending && !messages.some((m) => m.streaming) && (
          <div className="thinking-block is-streaming role-executor">
            <div className="tb-header">
              <span className="tb-caret" aria-hidden>▾</span>
              <span className="role-chip chip-executor">Thinking</span>
              <span className="streaming-dot" aria-label="thinking" />
            </div>
          </div>
        )}
      </div>
      <div className="composer">
        <textarea
          placeholder={
            disabled
              ? "Open a project and ensure Ollama is reachable…"
              : "Ask the AI… (Ctrl+Enter to send)"
          }
          value={input}
          onChange={(e) => setInput(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === "Enter" && (e.ctrlKey || e.metaKey)) {
              e.preventDefault();
              void send();
            }
          }}
          disabled={disabled || sending}
        />
        {sending ? (
          <button className="danger" onClick={() => void api.cancelChat()}>
            Stop
          </button>
        ) : (
          <button
            className="primary"
            disabled={disabled || input.trim().length === 0}
            onClick={() => void send()}
          >
            Send
          </button>
        )}
      </div>
    </div>
  );
}

function renderMessage(
  m: ChatMessage,
  tier: Tier,
  onOpenSettings: () => void,
) {
  if (tier === "user") {
    return (
      <div key={m.id} className="msg role-user">
        <div className="msg-role">you</div>
        <div className="msg-body">{m.content}</div>
      </div>
    );
  }
  if (tier === "system") {
    if (m.kind === "warn_action") {
      // F-4: executor-unparsed notice. Click opens Settings so the
      // user can swap to a larger executor model. Dedicated class
      // adds a second line of weight so this cannot get mistaken
      // for a routine info pill (which is the whole point — the
      // old "review skipped (unparsed)" label was invisible).
      return (
        <SystemAction
          key={m.id}
          icon="⚠"
          tone="warn"
          text={m.content}
          onClick={onOpenSettings}
          title="Open Settings to change the executor model"
        />
      );
    }
    return (
      <SystemAction
        key={m.id}
        icon="•"
        tone={/error/i.test(m.content) ? "error" : "info"}
        text={m.content}
      />
    );
  }
  const duration =
    m.ended_at != null && m.started_at != null
      ? m.ended_at - m.started_at
      : undefined;
  if (tier === "final") {
    return (
      <FinalAnswerBubble
        key={m.id}
        content={m.content}
        streaming={m.streaming}
        provider={m.provider}
        model={m.model}
        toolCalls={m.tool_calls}
        toolResults={m.tool_results}
      />
    );
  }
  const role: AgentRole = (m.streaming_role ?? "executor") as AgentRole;
  return (
    <ThinkingBlock
      key={m.id}
      role={role}
      content={m.content}
      streaming={m.streaming ?? false}
      provider={m.provider}
      model={m.model}
      durationMs={duration}
      toolCalls={m.tool_calls}
      toolResults={m.tool_results}
    />
  );
}
