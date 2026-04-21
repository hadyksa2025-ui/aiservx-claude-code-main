import { useEffect, useRef, useState, useCallback } from "react";
import { api, onEvent } from "../api";

interface TerminalProps {
  projectDir: string | null;
  terminalId: string;
  /**
   * When true, this terminal renders the AI agent's tool stream. The
   * input box is hidden (the user doesn't drive this tab) and Ctrl+C
   * is ignored — agent cancellation goes through the goal cancel flow,
   * not through `terminalKill`.
   */
  agentMode?: boolean;
  onRunningChange?: (running: boolean) => void;
}

interface OutputLine {
  id: number;
  text: string;
  stream: "stdout" | "stderr";
}

export function Terminal({ projectDir, terminalId, agentMode = false, onRunningChange }: TerminalProps) {
  const [lines, setLines] = useState<OutputLine[]>([]);
  const [input, setInput] = useState("");
  const [running, setRunning] = useState(false);
  const [history, setHistory] = useState<string[]>([]);
  const [historyIndex, setHistoryIndex] = useState(-1);
  const outputRef = useRef<HTMLDivElement>(null);
  const inputRef = useRef<HTMLInputElement>(null);
  const lineIdRef = useRef(0);

  useEffect(() => {
    onRunningChange?.(running);
  }, [running, onRunningChange]);

  useEffect(() => {
    const unlistens: Array<Promise<() => void>> = [];

    unlistens.push(
      onEvent<{ terminal_id: string; stream: string; data: string }>(
        "terminal:output",
        (p) => {
          if (p.terminal_id !== terminalId) return;
          setLines((prev) => [
            ...prev,
            {
              id: lineIdRef.current++,
              stream: p.stream as "stdout" | "stderr",
              text: p.data,
            },
          ]);
        },
      ),
    );

    unlistens.push(
      onEvent<{ terminal_id: string; exit_code: number }>("terminal:done", (p) => {
        if (p.terminal_id !== terminalId) return;
        setRunning(false);
      }),
    );

    return () => {
      for (const p of unlistens) {
        void p.then((fn) => fn());
      }
    };
  }, [terminalId]);

  useEffect(() => {
    if (outputRef.current) {
      outputRef.current.scrollTop = outputRef.current.scrollHeight;
    }
  }, [lines]);

  const runCommand = useCallback(async (cmd: string) => {
    if (!projectDir || !cmd.trim()) return;

    setRunning(true);
    setHistory((prev) => {
      const next = [...prev, cmd];
      if (next.length > 100) {
        return next.slice(-100);
      }
      return next;
    });
    setHistoryIndex(-1);
    setInput("");

    try {
      await api.runCmdStream(terminalId, projectDir, cmd);
    } catch (e) {
      setLines((prev) => [
        ...prev,
        { id: lineIdRef.current++, stream: "stderr", text: `Error: ${e}` },
      ]);
    }
    setRunning(false);
  }, [projectDir, terminalId]);

  const handleKeyDown = (e: React.KeyboardEvent) => {
    if (e.key === "Enter" && !running) {
      runCommand(input);
    } else if (e.key === "ArrowUp") {
      e.preventDefault();
      if (history.length > 0) {
        const newIndex = historyIndex < history.length - 1 ? historyIndex + 1 : historyIndex;
        setHistoryIndex(newIndex);
        setInput(history[history.length - 1 - newIndex] || "");
      }
    } else if (e.key === "ArrowDown") {
      e.preventDefault();
      if (historyIndex > 0) {
        const newIndex = historyIndex - 1;
        setHistoryIndex(newIndex);
        setInput(history[history.length - 1 - newIndex] || "");
      } else if (historyIndex === 0) {
        setHistoryIndex(-1);
        setInput("");
      }
    } else if (e.key === "c" && e.ctrlKey) {
      e.preventDefault();
      if (running) {
        void api.terminalKill(terminalId);
      }
      setRunning(false);
      setInput("");
    }
  };

  return (
    <div className={`terminal${agentMode ? " terminal-agent" : ""}`}>
      <div className="terminal-output" ref={outputRef}>
        {agentMode && lines.length === 0 && (
          <div className="terminal-line terminal-agent-hint">
            Agent is idle. AI tool output (read_file, write_file, run_cmd, …) will stream here live.
          </div>
        )}
        {lines.map((line) => (
          <div
            key={line.id}
            className={`terminal-line ${line.stream === "stderr" ? "terminal-stderr" : ""}`}
          >
            {line.stream === "stderr" ? <span className="terminal-prefix">$ </span> : null}
            {line.text}
          </div>
        ))}
        {running && !agentMode && (
          <div className="terminal-line terminal-running">
            <span className="terminal-cursor">$ </span>
            <span className="terminal-spinner">...</span>
          </div>
        )}
      </div>
      {!agentMode && (
        <div className="terminal-input-line">
          <span className="terminal-prompt">$</span>
          <input
            ref={inputRef}
            type="text"
            className="terminal-input"
            value={input}
            onChange={(e) => setInput(e.target.value)}
            onKeyDown={handleKeyDown}
            disabled={!projectDir || running}
            placeholder={projectDir ? "Type command..." : "Open a project first"}
            autoFocus
          />
        </div>
      )}
    </div>
  );
}
