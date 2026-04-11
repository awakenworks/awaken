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
    <aside className="rounded-3xl border border-slate-200 bg-white p-5 shadow-sm xl:sticky xl:top-6">
      <div className="flex items-start justify-between gap-3">
        <div>
          <p className="text-xs font-semibold uppercase tracking-[0.22em] text-cyan-700">
            Draft Preview
          </p>
          <h3 className="mt-2 text-xl font-semibold text-slate-950">Talk To This Agent</h3>
        </div>
        <button
          type="button"
          onClick={handleReset}
          className="rounded-xl border border-slate-300 px-3 py-2 text-sm font-medium text-slate-700 transition hover:border-slate-400 hover:bg-slate-50"
        >
          New Session
        </button>
      </div>

      <p className="mt-3 text-sm leading-6 text-slate-500">
        Each message runs against the current draft in the editor, including unsaved
        plugin settings and prompt changes.
      </p>

      <div className="mt-4 rounded-2xl bg-slate-950 px-4 py-3 text-xs text-slate-200">
        <div className="flex items-center justify-between gap-3">
          <span className="uppercase tracking-[0.18em] text-slate-400">Preview Agent</span>
          <span className="rounded-full bg-slate-800 px-2 py-0.5 text-[11px] text-slate-300">
            {previewDraft.id}
          </span>
        </div>
        <div className="mt-2 break-all text-slate-100">
          model={previewDraft.model_id || "unassigned"}
        </div>
      </div>

      {blockedReason ? (
        <div className="mt-4 rounded-2xl border border-amber-200 bg-amber-50 px-4 py-3 text-sm text-amber-700">
          {blockedReason}
        </div>
      ) : null}

      {error ? (
        <div className="mt-4 rounded-2xl border border-rose-200 bg-rose-50 px-4 py-3 text-sm text-rose-700">
          {error.message}
        </div>
      ) : null}

      <div className="mt-4 flex min-h-[26rem] flex-col overflow-hidden rounded-3xl border border-slate-200 bg-slate-50">
        <div className="min-h-0 flex-1 overflow-y-auto px-4 py-4">
          {messages.length === 0 ? (
            <div className="rounded-2xl border border-dashed border-slate-200 bg-white px-4 py-5 text-sm text-slate-500">
              Start a conversation to validate the draft agent behavior before publishing.
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
                      "max-w-[92%] rounded-2xl px-4 py-3 text-sm leading-6 shadow-sm",
                      isUser
                        ? "ml-auto bg-slate-900 text-white"
                        : "bg-white text-slate-800",
                    ].join(" ")}
                  >
                    <div
                      className={[
                        "mb-1 text-[11px] font-semibold uppercase tracking-[0.18em]",
                        isUser ? "text-slate-300" : "text-slate-500",
                      ].join(" ")}
                    >
                      {isUser ? "You" : "Agent"}
                    </div>
                    <div className="whitespace-pre-wrap break-words">{text}</div>
                  </div>
                );
              })}
              {busy ? (
                <div className="max-w-[92%] rounded-2xl bg-white px-4 py-3 text-sm text-slate-500 shadow-sm">
                  Agent is thinking...
                </div>
              ) : null}
            </div>
          )}
        </div>

        <form
          onSubmit={handleSubmit}
          className="border-t border-slate-200 bg-white px-4 py-4"
        >
          <textarea
            value={input}
            onChange={(event) => setInput(event.target.value)}
            rows={4}
            disabled={Boolean(blockedReason) || busy}
            placeholder="Ask the draft agent to solve something, call a tool, or explain its next step..."
            className="w-full rounded-2xl border border-slate-300 bg-white px-4 py-3 text-sm text-slate-900 outline-none transition focus:border-slate-500 disabled:bg-slate-100 disabled:text-slate-500"
          />
          <div className="mt-3 flex items-center justify-between gap-3">
            <div className="text-xs text-slate-500">
              Session ID: <span className="font-mono">{sessionId}</span>
            </div>
            <button
              type="submit"
              disabled={Boolean(blockedReason) || busy || input.trim().length === 0}
              className="rounded-xl bg-slate-950 px-4 py-2 text-sm font-medium text-white transition hover:bg-slate-800 disabled:cursor-not-allowed disabled:opacity-60"
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
