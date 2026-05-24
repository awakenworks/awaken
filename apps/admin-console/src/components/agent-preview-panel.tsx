import { DefaultChatTransport } from "ai";
import { useChat, type UIMessage } from "@ai-sdk/react";
import { useEffect, useMemo, useRef, useState, type FormEvent, type ReactNode } from "react";
import { agentPreviewRunUrl, type AgentSpec } from "@/lib/config-api";
import { adminAuthHeaders } from "@/lib/api/http";
import {
  redactSecretString,
  redactSecretsForDisplay,
  safeErrorMessage,
} from "@/lib/agent-editor-helpers";
import { RecentTracesDrawer } from "@/components/recent-traces-drawer";

interface AgentPreviewPanelProps {
  draft: AgentSpec;
  traceAgentId?: string;
}

export function AgentPreviewPanel({
  draft,
  traceAgentId: rawTraceAgentId,
}: AgentPreviewPanelProps) {
  const [sessionId, setSessionId] = useState(() => makePreviewSessionId());
  const [input, setInput] = useState("");
  const [lastLatencyMs, setLastLatencyMs] = useState<number | null>(null);
  const [tracesOpen, setTracesOpen] = useState(false);
  const sendStartedAtRef = useRef<number | null>(null);
  const previewDraft = normalizePreviewAgent(draft);
  const traceAgentId = rawTraceAgentId?.trim() ?? "";
  const canShowRecentRuns = traceAgentId.length > 0;
  const draftRef = useRef(previewDraft);

  useEffect(() => {
    draftRef.current = previewDraft;
  }, [previewDraft]);

  useEffect(() => {
    if (!canShowRecentRuns) {
      setTracesOpen(false);
    }
  }, [canShowRecentRuns]);

  const transport = useMemo(
    () =>
      new DefaultChatTransport({
        api: agentPreviewRunUrl(),
        prepareSendMessagesRequest: ({ messages }) => ({
          // Resolve auth at send time so a freshly-saved bearer is used.
          headers: adminAuthHeaders(),
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
    <aside className="rounded-sm border border-line bg-surface p-4 shadow-sm xl:sticky xl:top-6">
      <div className="flex items-baseline justify-between gap-3">
        <h3 className="text-sm font-semibold text-fg-strong">
          Sandbox <span className="font-normal text-fg-soft">runs against current draft</span>
        </h3>
        <div className="flex items-center gap-3">
          {canShowRecentRuns ? (
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
        agentId={traceAgentId}
        open={tracesOpen}
        onClose={() => setTracesOpen(false)}
      />

      <div className="mt-3 rounded-sm bg-code-bg px-3 py-2 font-mono text-[11px] leading-5 text-code-fg">
        <span className="text-code-fg/70">id=</span>
        {previewDraft.id} <span className="text-code-fg/70">model=</span>
        {previewDraft.model_id || "unassigned"}
      </div>

      <PreviewStatsBar messages={messages} latencyMs={lastLatencyMs} busy={busy} />

      {blockedReason ? (
        <div className="mt-4 rounded-sm border border-tone-warn/35 bg-tone-warn/10 px-4 py-3 text-sm text-tone-warn">
          {blockedReason}
        </div>
      ) : null}

      {error ? (
        <div className="mt-4 rounded-sm border border-tone-error/30 bg-tone-error/10 px-4 py-3 text-sm text-tone-error">
          {safeErrorMessage(error)}
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
                      "max-w-[92%] rounded-sm px-4 py-3 text-sm leading-6 shadow-sm",
                      isUser ? "ml-auto bg-accent text-accent-text" : "bg-surface text-fg",
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
                <div className="max-w-[92%] rounded-sm bg-surface px-4 py-3 text-sm text-fg-soft shadow-sm">
                  Agent is thinking...
                </div>
              ) : null}
            </div>
          )}
        </div>

        <form onSubmit={handleSubmit} className="border-t border-line bg-surface px-4 py-4">
          <textarea
            value={input}
            onChange={(event) => setInput(event.target.value)}
            rows={4}
            disabled={Boolean(blockedReason) || busy}
            placeholder="Type a message…"
            className="w-full rounded-sm border border-line-strong bg-surface px-4 py-3 text-sm text-fg outline-none transition focus:border-fg disabled:bg-muted disabled:text-fg-soft"
          />
          <div className="mt-3 flex items-center justify-between gap-3">
            <div title={`Session ID: ${sessionId}`} className="font-mono text-[10px] text-fg-faint">
              session · {sessionId.slice(-8)}
            </div>
            <button
              type="submit"
              disabled={Boolean(blockedReason) || busy || input.trim().length === 0}
              className="rounded-sm bg-accent px-4 py-2 text-sm font-medium text-accent-text transition hover:opacity-90 disabled:cursor-not-allowed disabled:opacity-60"
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
    <div className="mt-3 grid grid-cols-3 gap-px overflow-hidden rounded-sm border border-line bg-line text-[11px]">
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
      <div className="text-[10px] font-medium uppercase tracking-eyebrow text-fg-soft">{label}</div>
      <div className="mt-0.5 font-mono text-sm font-semibold text-fg-strong">{value}</div>
    </div>
  );
}

export function MessageParts({ message }: { message: UIMessage }) {
  const rendered: ReactNode[] = [];
  const unknownTypes: string[] = [];
  for (const [index, part] of message.parts.entries()) {
    if (!part || typeof part !== "object" || !("type" in part)) continue;
    if (part.type === "step-start") {
      // Visual separator between agent turns; no content.
      continue;
    }
    if (part.type === "text") {
      if (typeof part.text === "string" && part.text.length > 0) {
        rendered.push(
          <div key={index} className="whitespace-pre-wrap break-words">
            {redactSecretString(part.text)}
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
          className="rounded-sm border border-line bg-soft px-3 py-2 text-xs text-fg-soft"
        >
          <summary className="cursor-pointer text-[10px] font-medium uppercase tracking-eyebrow text-fg-soft">
            Reasoning
          </summary>
          <pre className="mt-2 whitespace-pre-wrap break-words font-mono text-[11px] text-fg">
            {redactSecretString(text)}
          </pre>
        </details>,
      );
      continue;
    }
    if (part.type === "dynamic-tool" || part.type.startsWith("tool-")) {
      rendered.push(<ToolInvocation key={index} part={part as ToolPart} />);
      continue;
    }
    // Anything we don't render directly — metadata, source, file, future
    // SDK additions — gets collected into a single collapsible debug
    // fallback rather than producing an empty bubble.
    unknownTypes.push(part.type);
  }
  if (unknownTypes.length > 0) {
    rendered.push(
      <details
        key="__unknown_parts"
        data-testid="message-unknown-parts"
        className="rounded-sm border border-dashed border-line bg-surface px-3 py-2 text-xs text-fg-soft"
      >
        <summary className="cursor-pointer text-[10px] font-medium uppercase tracking-eyebrow text-fg-soft">
          {unknownTypes.length} unrecognized part
          {unknownTypes.length === 1 ? "" : "s"}
        </summary>
        <ul className="mt-2 list-disc pl-5 font-mono text-[11px] text-fg-soft">
          {unknownTypes.map((typeName, i) => (
            <li key={i}>{typeName}</li>
          ))}
        </ul>
      </details>,
    );
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
    <details className="rounded-sm border border-line bg-soft text-xs">
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
        <div className="text-[10px] font-medium uppercase tracking-eyebrow text-fg-soft">Input</div>
        <pre className="mt-1 max-h-48 overflow-auto rounded-sm bg-code-bg p-2 font-mono text-[11px] text-code-fg">
          {formatJson(part.input)}
        </pre>
        {part.errorText ? (
          <>
            <div className="mt-2 text-[10px] font-medium uppercase tracking-eyebrow text-tone-error">
              Error
            </div>
            <pre className="mt-1 max-h-48 overflow-auto rounded-sm border border-tone-error/30 bg-tone-error/10 p-2 font-mono text-[11px] text-tone-error">
              {/* R12 #3 — tool error text often quotes the offending
                  Authorization header / api_key in plaintext. Scrub
                  before rendering. */}
              {redactSecretString(part.errorText)}
            </pre>
          </>
        ) : part.output !== undefined ? (
          <>
            <div className="mt-2 text-[10px] font-medium uppercase tracking-eyebrow text-fg-soft">
              Output
            </div>
            <pre className="mt-1 max-h-48 overflow-auto rounded-sm bg-code-bg p-2 font-mono text-[11px] text-code-fg">
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

export function hasRenderableContent(message: UIMessage): boolean {
  // Mirrors MessageParts: known payloads and unknown SDK parts render; empty
  // text/reasoning and step separators do not.
  return message.parts.some(isDisplayablePart);
}

function isDisplayablePart(part: unknown): boolean {
  if (!part || typeof part !== "object" || !("type" in part)) return false;
  const typed = part as { type: string; text?: unknown };
  if (typed.type === "step-start") return false;
  if (typed.type === "text" || typed.type === "reasoning") {
    return typeof typed.text === "string" && typed.text.length > 0;
  }
  if (typed.type === "dynamic-tool" || typed.type.startsWith("tool-")) {
    return true;
  }
  // Anything else lands in the unrecognized-parts debug fallback —
  // worth showing the bubble for it.
  return true;
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
  // R12 #3 — string outputs go through pattern-based credential
  // scrubbing. A tool can return a plain-string payload (`"Authorization:
  // Bearer sk-..."`) or a structured object; without this branch the
  // object case was redacted by key but the string case rendered raw.
  if (typeof value === "string") return redactSecretString(value);
  // R10 #5 — tool inputs/outputs can carry API keys, authorization
  // headers, cookies, JWTs etc. Same redaction pipeline used by audit /
  // trace / diff so a credential never lands in the preview DOM.
  const redacted = redactSecretsForDisplay(value);
  try {
    return JSON.stringify(redacted, null, 2);
  } catch {
    return String(redacted);
  }
}

export function normalizePreviewAgent(draft: AgentSpec): AgentSpec {
  // Strip provenance / locality fields before sending to the preview
  // endpoint. The server's `/v1/ai-sdk/agent-previews/runs` route returns
  // 400 if either `endpoint` or `registry` is present in the payload —
  // it forces the preview into the local-only resolve path so a crafted
  // draft can't route the run to an arbitrary remote backend or skip
  // registry-membership checks. Builtin / customized records loaded into
  // the editor naturally carry `registry` (and may carry `endpoint`), so
  // without this strip every preview of a registry-resident agent would
  // fail with `BadRequest`. The runtime preview is always local —
  // endpoint and registry are not meaningful here.
  // `String(x ?? "")` on every string field so a partial draft (missing `id` etc) doesn't crash.
  const { endpoint: _endpoint, registry: _registry, ...localDraft } = draft;
  return {
    ...localDraft,
    id: String(localDraft.id ?? "").trim() || "draft-preview",
    model_id: String(localDraft.model_id ?? "").trim(),
    system_prompt: String(localDraft.system_prompt ?? ""),
    plugin_ids: [...(localDraft.plugin_ids ?? [])],
    delegates: [...(localDraft.delegates ?? [])],
    sections: { ...(localDraft.sections ?? {}) },
  };
}

function makePreviewSessionId(): string {
  return `preview-${crypto.randomUUID()}`;
}
