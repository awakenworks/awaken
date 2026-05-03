import { DefaultChatTransport } from "ai";
import { useChat, type UIMessage } from "@ai-sdk/react";
import { useEffect, useMemo, useRef, useState, type FormEvent } from "react";
import { agentPreviewRunUrl, type AgentSpec } from "@/lib/config-api";

interface AgentPreviewPanelProps {
  draft: AgentSpec;
}

export function AgentPreviewPanel({ draft }: AgentPreviewPanelProps) {
  const [sessionId, setSessionId] = useState(() => makePreviewSessionId());
  const [input, setInput] = useState("");
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

  function handleSubmit(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    const text = input.trim();
    if (!text || busy || blockedReason) {
      return;
    }
    sendMessage({ text });
    setInput("");
  }

  function handleReset() {
    setSessionId(makePreviewSessionId());
    setMessages([]);
    setInput("");
  }

  return (
    <aside className="rounded-md border border-line bg-surface p-4 shadow-sm xl:sticky xl:top-6">
      <div className="flex items-baseline justify-between gap-3">
        <h3 className="text-sm font-semibold text-fg-strong">
          Sandbox <span className="font-normal text-fg-soft">runs against current draft</span>
        </h3>
        <button
          type="button"
          onClick={handleReset}
          className="text-xs font-medium text-fg-soft transition hover:text-fg-strong"
        >
          Reset
        </button>
      </div>

      <div className="mt-3 rounded-md bg-code-bg px-3 py-2 font-mono text-[11px] leading-5 text-code-fg">
        <span className="text-code-fg/70">id=</span>{previewDraft.id}{" "}
        <span className="text-code-fg/70">model=</span>{previewDraft.model_id || "unassigned"}
      </div>

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
                const text = extractMessageText(message);
                if (!text) {
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
                    <div className="whitespace-pre-wrap break-words">{text}</div>
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

function extractMessageText(message: UIMessage): string {
  return message.parts
    .flatMap((part) => {
      if (!part || typeof part !== "object" || !("type" in part)) {
        return [];
      }
      if (part.type !== "text") {
        return [];
      }
      return typeof part.text === "string" && part.text.trim().length > 0
        ? [part.text]
        : [];
    })
    .join("");
}

function makePreviewSessionId(): string {
  return `preview-${crypto.randomUUID()}`;
}
