import { DefaultChatTransport } from "ai";
import { useChat, type UIMessage } from "@ai-sdk/react";
import { useEffect, useMemo, useRef, useState, type FormEvent, type ReactNode } from "react";
import { agentPreviewRunUrl, type AgentSpec } from "@/lib/config-api";
import { RecentTracesDrawer } from "@/components/recent-traces-drawer";

interface AgentPreviewPanelProps {
  draft: AgentSpec;
}

export function AgentPreviewPanel({ draft }: AgentPreviewPanelProps) {
  const [sessionId, setSessionId] = useState(() => makePreviewSessionId());
  const [input, setInput] = useState("");
  const [lastLatencyMs, setLastLatencyMs] = useState<number | null>(null);
  const [tracesOpen, setTracesOpen] = useState(false);
  const sendStartedAtRef = useRef<number | null>(null);
  const previewDraft = normalizePreviewAgent(draft);
  const draftRef = useRef(previewDraft);

  useEffect(() => {
    draftRef.current = previewDraft;
  }, [previewDraft]);

  const transport = useMemo(
    () =>
      new DefaultChatTransport({
        api: agentPreviewRunUrl(),
        prepareSendMessagesRequest: ({ messages }) => ({
          body: {
            threadId: sessionId,
            messages,
            agent: draftRef.current,
          },
        }),
      }),
    [sessionId],
  );

  const { messages, sendMessage, setMessages, status, error } = useChat({
    id: `agent-preview:${sessionId}`,
    transport,
  });

  const blockedReason = previewDraft.model_id.trim()
    ? null
    : "Select a model before starting a preview conversation.";
  const busy = status === "submitted" || status === "streaming";

  useEffect(() => {
    if (!busy && sendStartedAtRef.current !== null) {
      setLastLatencyMs(Date.now() - sendStartedAtRef.current);
      sendStartedAtRef.current = null;
    }
  }, [busy]);

  function handleSubmit(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    const text = input.trim();
    if (!text || busy || blockedReason) {
      return;
    }
    sendStartedAtRef.current = Date.now();
    setLastLatencyMs(null);
    sendMessage({ text });
    setInput("");
  }

  function handleReset() {
    setSessionId(makePreviewSessionId());
    setMessages([]);
    setInput("");
    sendStartedAtRef.current = null;
    setLastLatencyMs(null);
  }

  return (
    <aside className="rounded-md border border-line bg-surface p-4 shadow-sm xl:sticky xl:top-6">
      <div className="flex items-baseline justify-between gap-3">
        <h3 className="text-sm font-semibold text-fg-strong">
          Sandbox <span className="font-normal text-fg-soft">runs against current draft</span>
        </h3>
        <div className="flex items-center gap-3">
          {previewDraft.id.trim().length > 0 ? (
            <button
              type="button"
              onClick={() => setTracesOpen(true)}
              data-testid="open-recent-traces"
              className="text-xs font-medium text-fg-soft transition hover:text-fg-strong"
            >
              Recent runs
            </button>
          ) : null}
          <button
            type="button"
            onClick={handleReset}
            className="text-xs font-medium text-fg-soft transition hover:text-fg-strong"
          >
            Reset
          </button>
        </div>
      </div>

      <RecentTracesDrawer
        agentId={previewDraft.id}
        open={tracesOpen}
        onClose={() => setTracesOpen(false)}
      />

      <div className="mt-3 rounded-md bg-code-bg px-3 py-2 font-mono text-[11px] leading-5 text-code-fg">
        <span className="text-code-fg/70">id=</span>{previewDraft.id}{" "}
        <span className="text-code-fg/70">model=</span>{previewDraft.model_id || "unassigned"}
      </div>

      <PreviewStatsBar messages={messages} latencyMs={lastLatencyMs} busy={busy} />

      {blockedReason ? (
        <div className="mt-4 rounded-md border border-tone-warn/35 bg-tone-warn/10 px-4 py-3 text-sm text-tone-warn">
          {blockedReason}
        </div>
      ) : null}

      {error ? (
        <div className="mt-4 rounded-md border border-tone-error/30 bg-tone-error/10 px-4 py-3 text-sm text-tone-error">
          {error.message}
        </div>
      ) : null}

      <div className="mt-4 flex min-h-[26rem] flex-col overflow-hidden rounded-lg border border-line bg-soft">
        <div className="min-h-0 flex-1 overflow-y-auto px-4 py-4">
          {messages.length === 0 ? (
            <div className="flex h-full items-center justify-center text-center text-xs text-fg-faint">
              No messages yet — send one below to validate the draft.
            </div>
          ) : (
            <div className="space-y-3">
              {messages.map((message) => {
                if (!hasRenderableContent(message)) {
                  return null;
                }
                const isUser = message.role === "user";
                return (
                  <div
                    key={message.id}
                    className={[
                      "max-w-[92%] rounded-md px-4 py-3 text-sm leading-6 shadow-sm",
                      isUser
                        ? "ml-auto bg-accent text-accent-text"
                        : "bg-surface text-fg",
                    ].join(" ")}
                  >
                    <div
                      className={[
                        "mb-1 text-[11px] font-semibold uppercase tracking-[0.18em]",
                        isUser ? "text-fg-faint" : "text-fg-soft",
                      ].join(" ")}
                    >
                      {isUser ? "You" : "Agent"}
                    </div>
                    <MessageParts message={message} />
                  </div>
                );
              })}
              {busy ? (
                <div className="max-w-[92%] rounded-md bg-surface px-4 py-3 text-sm text-fg-soft shadow-sm">
                  Agent is thinking...
                </div>
              ) : null}
            </div>
          )}
        </div>

        <form
          onSubmit={handleSubmit}
          className="border-t border-line bg-surface px-4 py-4"
        >
          <textarea
            value={input}
            onChange={(event) => setInput(event.target.value)}
            rows={4}
            disabled={Boolean(blockedReason) || busy}
            placeholder="Type a message…"
            className="w-full rounded-md border border-line-strong bg-surface px-4 py-3 text-sm text-fg-strong outline-none transition focus:border-line-strong disabled:bg-muted disabled:text-fg-soft"
          />
          <div className="mt-3 flex items-center justify-between gap-3">
            <div
              title={`Session ID: ${sessionId}`}
              className="font-mono text-[10px] text-fg-faint"
            >
              session · {sessionId.slice(-8)}
            </div>
            <button
              type="submit"
              disabled={Boolean(blockedReason) || busy || input.trim().length === 0}
              className="rounded-xl bg-accent px-4 py-2 text-sm font-medium text-accent-text transition hover:opacity-90 disabled:cursor-not-allowed disabled:opacity-60"
            >
              {busy ? "Running..." : "Send"}
            </button>
          </div>
        </form>
      </div>
    </aside>
  );
}

function PreviewStatsBar({
  messages,
  latencyMs,
  busy,
}: {
  messages: UIMessage[];
  latencyMs: number | null;
  busy: boolean;
}) {
  const toolCalls = messages.reduce((acc, message) => acc + countToolCalls(message), 0);
  const latencyLabel =
    latencyMs !== null
      ? latencyMs >= 1000
        ? `${(latencyMs / 1000).toFixed(2)}s`
        : `${latencyMs}ms`
      : busy
        ? "running…"
        : "—";

  return (
    <div className="mt-3 grid grid-cols-3 gap-px overflow-hidden rounded-md border border-line bg-line text-[11px]">
      <StatCell label="Messages" value={String(messages.length)} />
      <StatCell label="Tool calls" value={String(toolCalls)} />
      <StatCell
        label="Last turn"
        value={latencyLabel}
        title="Wall-clock time from send to the model going idle"
      />
    </div>
  );
}

function StatCell({ label, value, title }: { label: string; value: string; title?: string }) {
  return (
    <div className="bg-surface px-3 py-2" title={title}>
      <div className="text-[10px] font-medium uppercase tracking-eyebrow text-fg-soft">
        {label}
      </div>
      <div className="mt-0.5 font-mono text-sm font-semibold text-fg-strong">{value}</div>
    </div>
  );
}

function MessageParts({ message }: { message: UIMessage }) {
  const rendered: ReactNode[] = [];
  for (const [index, part] of message.parts.entries()) {
    if (!part || typeof part !== "object" || !("type" in part)) continue;
    if (part.type === "text") {
      if (typeof part.text === "string" && part.text.length > 0) {
        rendered.push(
          <div key={index} className="whitespace-pre-wrap break-words">
            {part.text}
          </div>,
        );
      }
      continue;
    }
    if (part.type === "reasoning") {
      const text = typeof part.text === "string" ? part.text : "";
      if (text.length === 0) continue;
      rendered.push(
        <details
          key={index}
          className="rounded-md border border-line bg-soft px-3 py-2 text-xs text-fg-soft"
        >
          <summary className="cursor-pointer text-[10px] font-medium uppercase tracking-eyebrow text-fg-soft">
            Reasoning
          </summary>
          <pre className="mt-2 whitespace-pre-wrap break-words font-mono text-[11px] text-fg">
            {text}
          </pre>
        </details>,
      );
      continue;
    }
    if (part.type === "dynamic-tool" || part.type.startsWith("tool-")) {
      rendered.push(<ToolInvocation key={index} part={part as ToolPart} />);
      continue;
    }
    if (part.type === "step-start") {
      // Visual separator between agent turns; no content.
      continue;
    }
  }
  if (rendered.length === 0) {
    return (
      <div className="text-xs italic text-fg-faint">
        (empty turn — no text or tool parts emitted)
      </div>
    );
  }
  return <div className="space-y-2">{rendered}</div>;
}

interface ToolPart {
  type: string;
  toolName?: string;
  toolCallId?: string;
  state?: string;
  input?: unknown;
  output?: unknown;
  errorText?: string;
}

function ToolInvocation({ part }: { part: ToolPart }) {
  const name = part.toolName ?? part.type.replace(/^tool-/, "") ?? "tool";
  const state = part.state ?? "input-streaming";
  const tone = TOOL_STATE_TONE[state] ?? "neutral";
  return (
    <details className="rounded-md border border-line bg-soft text-xs">
      <summary className="flex cursor-pointer flex-wrap items-center gap-2 px-3 py-2">
        <span
          className={[
            "rounded-pill px-1.5 py-0.5 text-[10px] font-medium uppercase tracking-eyebrow",
            TOOL_TONE_STYLE[tone],
          ].join(" ")}
        >
          {TOOL_STATE_LABEL[state] ?? state}
        </span>
        <span className="font-mono text-fg-strong">{name}</span>
        {part.toolCallId ? (
          <span className="ml-auto font-mono text-[10px] text-fg-faint">
            {part.toolCallId.slice(-8)}
          </span>
        ) : null}
      </summary>
      <div className="border-t border-line px-3 py-2">
        <div className="text-[10px] font-medium uppercase tracking-eyebrow text-fg-soft">
          Input
        </div>
        <pre className="mt-1 max-h-48 overflow-auto rounded-md bg-code-bg p-2 font-mono text-[11px] text-code-fg">
          {formatJson(part.input)}
        </pre>
        {part.errorText ? (
          <>
            <div className="mt-2 text-[10px] font-medium uppercase tracking-eyebrow text-tone-error">
              Error
            </div>
            <pre className="mt-1 max-h-48 overflow-auto rounded-md border border-tone-error/30 bg-tone-error/10 p-2 font-mono text-[11px] text-tone-error">
              {part.errorText}
            </pre>
          </>
        ) : part.output !== undefined ? (
          <>
            <div className="mt-2 text-[10px] font-medium uppercase tracking-eyebrow text-fg-soft">
              Output
            </div>
            <pre className="mt-1 max-h-48 overflow-auto rounded-md bg-code-bg p-2 font-mono text-[11px] text-code-fg">
              {formatJson(part.output)}
            </pre>
          </>
        ) : null}
      </div>
    </details>
  );
}

const TOOL_STATE_LABEL: Record<string, string> = {
  "input-streaming": "Calling",
  "input-available": "Calling",
  "approval-requested": "Awaiting approval",
  "approval-responded": "Approved",
  "output-available": "Done",
  "output-error": "Error",
  "output-denied": "Denied",
};

const TOOL_STATE_TONE: Record<string, "neutral" | "info" | "success" | "warn" | "error"> = {
  "input-streaming": "info",
  "input-available": "info",
  "approval-requested": "warn",
  "approval-responded": "info",
  "output-available": "success",
  "output-error": "error",
  "output-denied": "warn",
};

const TOOL_TONE_STYLE: Record<"neutral" | "info" | "success" | "warn" | "error", string> = {
  neutral: "bg-muted text-fg-soft",
  info: "bg-blue-100 text-blue-800 dark:bg-blue-900/30 dark:text-blue-300",
  success: "bg-tone-success/15 text-tone-success",
  warn: "bg-tone-warn/15 text-tone-warn",
  error: "bg-tone-error/15 text-tone-error",
};

function hasRenderableContent(message: UIMessage): boolean {
  return message.parts.some((part) => {
    if (!part || typeof part !== "object" || !("type" in part)) return false;
    if (part.type === "step-start") return false;
    if (part.type === "text") {
      return typeof part.text === "string" && part.text.length > 0;
    }
    return true;
  });
}

function countToolCalls(message: UIMessage): number {
  return message.parts.reduce((acc, part) => {
    if (!part || typeof part !== "object" || !("type" in part)) return acc;
    if (part.type === "dynamic-tool" || part.type.startsWith("tool-")) return acc + 1;
    return acc;
  }, 0);
}

function formatJson(value: unknown): string {
  if (value === undefined) return "(no value)";
  if (value === null) return "null";
  if (typeof value === "string") return value;
  try {
    return JSON.stringify(value, null, 2);
  } catch {
    return String(value);
  }
}

function normalizePreviewAgent(draft: AgentSpec): AgentSpec {
  return {
    ...draft,
    id: draft.id.trim() || "draft-preview",
    model_id: String(draft.model_id ?? "").trim(),
    system_prompt: String(draft.system_prompt ?? ""),
    plugin_ids: [...(draft.plugin_ids ?? [])],
    delegates: [...(draft.delegates ?? [])],
    sections: { ...(draft.sections ?? {}) },
  };
}

function makePreviewSessionId(): string {
  return `preview-${crypto.randomUUID()}`;
}
