import { useCallback, useEffect, useRef, useState } from "react";
import { api } from "../api";
import type { ChatMessage } from "../types";

type Props = {
  projectDir: string | null;
  messages: ChatMessage[];
  setMessages: React.Dispatch<React.SetStateAction<ChatMessage[]>>;
  disabled: boolean;
};

function uid() {
  return Math.random().toString(36).slice(2, 10);
}

export function Chat({ projectDir, messages, setMessages, disabled }: Props) {
  const [input, setInput] = useState("");
  const [sending, setSending] = useState(false);
  const scrollRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    scrollRef.current?.scrollTo({ top: scrollRef.current.scrollHeight });
  }, [messages]);

  const send = useCallback(async () => {
    if (!projectDir || sending || input.trim().length === 0) return;
    const userMsg: ChatMessage = { id: uid(), role: "user", content: input.trim() };
    const historySnapshot = [...messages, userMsg];
    setMessages(historySnapshot);
    setInput("");
    setSending(true);
    try {
      const resp = await api.sendChat(projectDir, userMsg.content, messages);
      const asst: ChatMessage = {
        id: uid(),
        role: "assistant",
        content: resp.assistant || "(no response)",
        tool_calls: resp.tool_calls,
        tool_results: resp.tool_results,
      };
      setMessages((prev) => [...prev, asst]);
    } catch (e) {
      setMessages((prev) => [
        ...prev,
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

  return (
    <div className="chat">
      <div className="chat-messages" ref={scrollRef}>
        {messages.length === 0 && (
          <div className="empty-state">
            Ask the AI to read, edit, or run commands in your project.
          </div>
        )}
        {messages.map((m) => (
          <div key={m.id} className={"msg role-" + m.role}>
            <div className="msg-role">{m.role}</div>
            <div>{m.content}</div>
            {m.tool_calls && m.tool_calls.length > 0 && (
              <div className="tool-summary">
                {m.tool_calls.length} tool call
                {m.tool_calls.length > 1 ? "s" : ""}:&nbsp;
                {m.tool_calls.map((t) => t.name).join(", ")}
              </div>
            )}
          </div>
        ))}
        {sending && (
          <div className="msg role-assistant">
            <div className="msg-role">assistant</div>
            <div style={{ color: "var(--fg-dim)" }}>thinking…</div>
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
        <button
          className="primary"
          disabled={disabled || sending || input.trim().length === 0}
          onClick={() => void send()}
        >
          {sending ? "…" : "Send"}
        </button>
      </div>
    </div>
  );
}
