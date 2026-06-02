import { DefaultChatTransport, type FileUIPart } from "ai";
import { useChat, type UIMessage } from "@ai-sdk/react";
import {
  createContext,
  useCallback,
  useContext,
  useEffect,
  useMemo,
  useRef,
  useState,
  type ChangeEvent,
  type FormEvent,
  type ReactNode,
} from "react";
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

type PreviewDisplayMode = "readable" | "json";

const SANDBOX_ACCEPTED_MEDIA =
  "image/*,audio/*,video/*,application/pdf,text/plain,text/markdown,application/json";
const MAX_SANDBOX_FILES = 4;
const MAX_SANDBOX_TOTAL_BYTES = 8 * 1024 * 1024;

// Lets the deeply-nested ToolInvocation resolve a tool-call approval without
// threading callbacks through the exported MessageParts signature.
interface ApprovalActions {
  onApprove?: (approvalId: string) => void;
  onDeny?: (approvalId: string) => void;
}
const ApprovalActionsContext = createContext<ApprovalActions>({});

export function AgentPreviewPanel({
  draft,
  traceAgentId: rawTraceAgentId,
}: AgentPreviewPanelProps) {
  const [sessionId, setSessionId] = useState(() => makePreviewSessionId());
  const [input, setInput] = useState("");
  const [selectedFiles, setSelectedFiles] = useState<File[]>([]);
  const [uploadError, setUploadError] = useState<string | null>(null);
  const [preparingFiles, setPreparingFiles] = useState(false);
  const [lastLatencyMs, setLastLatencyMs] = useState<number | null>(null);
  const [tracesOpen, setTracesOpen] = useState(false);
  const [displayMode, setDisplayMode] = useState<PreviewDisplayMode>("readable");
  const sendStartedAtRef = useRef<number | null>(null);
  const fileInputRef = useRef<HTMLInputElement>(null);
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

  const { messages, sendMessage, setMessages, status, error, addToolApprovalResponse } =
    useChat({
      id: `agent-preview:${sessionId}`,
      transport,
      // After the operator answers an approval prompt, re-send the conversation
      // so the suspended run resumes. Server-executed tools (providerExecuted)
      // arrive via the stream and must not trigger a resubmit.
      sendAutomaticallyWhen: ({ messages }) => {
        const lastAssistant = [...messages].reverse().find((m) => m.role === "assistant");
        if (!lastAssistant) return false;
        return lastAssistant.parts.some((part) => {
          if (!part || typeof part !== "object" || !("state" in part)) return false;
          if (
            "providerExecuted" in part &&
            (part as { providerExecuted?: boolean }).providerExecuted
          ) {
            return false;
          }
          const state = (part as { state?: string }).state;
          return (
            state === "output-available" ||
            state === "output-denied" ||
            state === "output-error" ||
            state === "approval-responded"
          );
        });
      },
    });

  const onApprove = useCallback(
    (approvalId: string) => {
      void addToolApprovalResponse({ id: approvalId, approved: true });
    },
    [addToolApprovalResponse],
  );
  const onDeny = useCallback(
    (approvalId: string) => {
      void addToolApprovalResponse({ id: approvalId, approved: false });
    },
    [addToolApprovalResponse],
  );

  const blockedReason = previewDraft.model_id.trim()
    ? null
    : "Select a model before starting a preview conversation.";
  const busy = preparingFiles || status === "submitted" || status === "streaming";
  const hasDraftInput = input.trim().length > 0 || selectedFiles.length > 0;

  useEffect(() => {
    if (!busy && sendStartedAtRef.current !== null) {
      setLastLatencyMs(Date.now() - sendStartedAtRef.current);
      sendStartedAtRef.current = null;
    }
  }, [busy]);

  async function handleSubmit(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    const text = input.trim();
    if ((!text && selectedFiles.length === 0) || busy || blockedReason) {
      return;
    }
    sendStartedAtRef.current = Date.now();
    setLastLatencyMs(null);
    setUploadError(null);
    setPreparingFiles(true);
    try {
      const files = await filesToFileParts(selectedFiles);
      const payload =
        files.length > 0
          ? text
            ? { text, files }
            : { files }
          : { text };
      setInput("");
      clearSelectedFiles();
      await sendMessage(payload);
    } catch (error) {
      sendStartedAtRef.current = null;
      setUploadError(safeErrorMessage(error));
    } finally {
      setPreparingFiles(false);
    }
  }

  function handleReset() {
    setSessionId(makePreviewSessionId());
    setMessages([]);
    setInput("");
    clearSelectedFiles();
    setUploadError(null);
    sendStartedAtRef.current = null;
    setLastLatencyMs(null);
  }

  function handleFileChange(event: ChangeEvent<HTMLInputElement>) {
    const files = Array.from(event.target.files ?? []);
    const validationError = validateSandboxFiles(files);
    if (validationError) {
      setUploadError(validationError);
      setSelectedFiles([]);
      event.target.value = "";
      return;
    }
    setUploadError(null);
    setSelectedFiles(files);
  }

  function clearSelectedFiles() {
    setSelectedFiles([]);
    if (fileInputRef.current) {
      fileInputRef.current.value = "";
    }
  }

  return (
    <ApprovalActionsContext.Provider value={{ onApprove, onDeny }}>
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

      <div className="mt-3 flex items-center justify-between gap-3">
        <div className="text-[10px] font-medium uppercase tracking-eyebrow text-fg-soft">
          Response view
        </div>
        <div
          role="group"
          aria-label="Sandbox response display"
          className="inline-flex overflow-hidden rounded-sm border border-line bg-surface text-xs"
        >
          <button
            type="button"
            onClick={() => setDisplayMode("readable")}
            className={[
              "px-3 py-1.5 transition",
              displayMode === "readable"
                ? "bg-accent text-accent-text"
                : "text-fg-soft hover:bg-soft hover:text-fg",
            ].join(" ")}
          >
            Readable
          </button>
          <button
            type="button"
            onClick={() => setDisplayMode("json")}
            className={[
              "border-l border-line px-3 py-1.5 transition",
              displayMode === "json"
                ? "bg-accent text-accent-text"
                : "text-fg-soft hover:bg-soft hover:text-fg",
            ].join(" ")}
          >
            JSON
          </button>
        </div>
      </div>

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

      {uploadError ? (
        <div className="mt-4 rounded-sm border border-tone-error/30 bg-tone-error/10 px-4 py-3 text-sm text-tone-error">
          {uploadError}
        </div>
      ) : null}

      <div className="mt-2 flex min-h-[26rem] flex-col overflow-hidden rounded-lg border border-line bg-soft">
        <div className="min-h-0 flex-1 overflow-y-auto px-4 py-4">
          {messages.length === 0 ? (
            <div className="flex h-full items-center justify-center text-center text-xs text-fg-faint">
              No messages yet — send one below to validate the draft.
            </div>
          ) : (
            <div className="space-y-3">
              {messages.map((message) => {
                if (displayMode === "readable" && !hasRenderableContent(message)) {
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
                    <MessageBody message={message} mode={displayMode} />
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
          {selectedFiles.length > 0 ? (
            <div
              data-testid="sandbox-selected-files"
              className="mt-3 flex flex-wrap items-center gap-2 rounded-sm border border-line bg-soft px-3 py-2 text-xs text-fg-soft"
            >
              {selectedFiles.map((file) => (
                <span
                  key={`${file.name}:${file.size}:${file.lastModified}`}
                  className="max-w-full truncate rounded-pill border border-line bg-surface px-2 py-1"
                  title={`${file.name} · ${file.type || "application/octet-stream"} · ${formatBytes(file.size)}`}
                >
                  {file.name || "attachment"} · {formatBytes(file.size)}
                </span>
              ))}
              <button
                type="button"
                onClick={clearSelectedFiles}
                disabled={busy}
                className="ml-auto text-xs font-medium text-fg-soft transition hover:text-fg-strong disabled:cursor-not-allowed disabled:opacity-60"
              >
                Clear
              </button>
            </div>
          ) : null}
          <div className="mt-3 flex items-center justify-between gap-3">
            <div className="flex min-w-0 items-center gap-3">
              <label className="cursor-pointer rounded-sm border border-line bg-soft px-3 py-2 text-xs font-medium text-fg-soft transition hover:border-line-strong hover:text-fg-strong">
                Attach
                <input
                  ref={fileInputRef}
                  type="file"
                  accept={SANDBOX_ACCEPTED_MEDIA}
                  multiple
                  disabled={Boolean(blockedReason) || busy}
                  onChange={handleFileChange}
                  className="sr-only"
                />
              </label>
              <div title={`Session ID: ${sessionId}`} className="font-mono text-[10px] text-fg-faint">
                session · {sessionId.slice(-8)}
              </div>
            </div>
            <button
              type="submit"
              disabled={Boolean(blockedReason) || busy || !hasDraftInput}
              className="rounded-sm bg-accent px-4 py-2 text-sm font-medium text-accent-text transition hover:opacity-90 disabled:cursor-not-allowed disabled:opacity-60"
            >
              {busy ? "Running..." : "Send"}
            </button>
          </div>
        </form>
      </div>
    </aside>
    </ApprovalActionsContext.Provider>
  );
}

function MessageBody({ message, mode }: { message: UIMessage; mode: PreviewDisplayMode }) {
  if (mode === "json") {
    return <MessageJson message={message} />;
  }
  return <MessageParts message={message} />;
}

function MessageJson({ message }: { message: UIMessage }) {
  return (
    <pre
      data-testid="preview-message-json"
      className="max-h-96 overflow-auto rounded-sm bg-code-bg p-3 font-mono text-[11px] leading-5 text-code-fg"
    >
      {formatJson(message)}
    </pre>
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
  const runtimeParts: RuntimeDataPart[] = [];
  for (const [index, part] of message.parts.entries()) {
    if (!part || typeof part !== "object" || !("type" in part)) continue;
    if (part.type === "step-start") {
      // Visual separator between agent turns; no content.
      continue;
    }
    if (part.type === "text") {
      if (typeof part.text === "string" && part.text.length > 0) {
        rendered.push(
          <ReadableText key={index} text={part.text} />,
        );
      }
      continue;
    }
    if (part.type === "file") {
      rendered.push(<FileAttachment key={index} part={part as FilePart} />);
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
    if (isRuntimeDataPart(part)) {
      runtimeParts.push(part);
      continue;
    }
    // Anything we don't render directly — metadata, source, file, future
    // SDK additions — gets collected into a single collapsible debug
    // fallback rather than producing an empty bubble.
    unknownTypes.push(part.type);
  }
  if (runtimeParts.length > 0) {
    rendered.push(<RuntimeMetadataParts key="__runtime_metadata" parts={runtimeParts} />);
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

function ReadableText({ text }: { text: string }) {
  const parsed = parseStructuredText(text);
  if (parsed.ok) {
    return <StructuredPayload value={parsed.value} title="Structured response" />;
  }
  const renderedText = redactSecretString(text);
  return (
    <div className="space-y-2">
      {isScriptedResponseText(text) ? (
        <div
          data-testid="scripted-provider-warning"
          className="rounded-sm border border-tone-warn/35 bg-tone-warn/10 px-3 py-2 text-xs leading-5 text-tone-warn"
        >
          This reply came from the starter scripted provider. Select a model backed by a real
          provider before using Sandbox to judge prompt quality.
        </div>
      ) : null}
      <div className="whitespace-pre-wrap break-words">{renderedText}</div>
    </div>
  );
}

type RuntimeDataPartType = "data-run-info" | "data-inference-complete" | "data-state-snapshot";

interface RuntimeDataPart {
  type: RuntimeDataPartType;
  data?: unknown;
}

function RuntimeMetadataParts({ parts }: { parts: RuntimeDataPart[] }) {
  const runInfo = parts.find((part) => part.type === "data-run-info");
  const inference = [...parts].reverse().find((part) => part.type === "data-inference-complete");
  const stateSnapshots = parts.filter((part) => part.type === "data-state-snapshot");
  const latestState = stateSnapshots.at(-1);
  const inferenceData = asRecord(inference?.data);
  const lifecycle = asRecord(
    asRecord(asRecord(latestState?.data).extensions)?.["__runtime.run_lifecycle"],
  );
  const usage = asRecord(inferenceData.usage);
  const summary = [
    typeof lifecycle.status === "string" ? lifecycle.status : null,
    typeof inferenceData.model === "string" ? inferenceData.model : null,
    typeof usage.total_tokens === "number" ? `${usage.total_tokens} tokens` : null,
    typeof inferenceData.durationMs === "number" ? formatDuration(inferenceData.durationMs) : null,
  ].filter(Boolean);

  return (
    <details
      data-testid="preview-runtime-metadata"
      className="rounded-sm border border-line bg-soft px-3 py-2 text-xs text-fg-soft"
    >
      <summary className="cursor-pointer text-[10px] font-medium uppercase tracking-eyebrow text-fg-soft">
        Runtime metadata{summary.length > 0 ? ` · ${summary.join(" · ")}` : ""}
      </summary>
      <dl className="mt-2 grid gap-1.5">
        <MetadataRow label="Run" value={asRecord(runInfo?.data).runId} />
        <MetadataRow label="Thread" value={asRecord(runInfo?.data).threadId} />
        <MetadataRow label="Model" value={inferenceData.model} />
        <MetadataRow label="Duration" value={summaryValue(inferenceData.durationMs, formatDuration)} />
        <MetadataRow label="Usage" value={formatUsage(usage)} />
        <MetadataRow label="State snapshots" value={stateSnapshots.length} />
      </dl>
      <details className="mt-2 rounded-sm border border-line bg-code-bg">
        <summary className="cursor-pointer px-2 py-1 text-[10px] font-medium uppercase tracking-eyebrow text-code-fg/70">
          JSON data
        </summary>
        <pre className="max-h-48 overflow-auto border-t border-line p-2 font-mono text-[11px] leading-5 text-code-fg">
          {formatJson(parts)}
        </pre>
      </details>
    </details>
  );
}

function MetadataRow({ label, value }: { label: string; value: unknown }) {
  if (value === undefined || value === null || value === "") return null;
  return (
    <div className="grid grid-cols-[6rem_1fr] gap-2">
      <dt className="text-fg-soft">{label}</dt>
      <dd className="min-w-0 break-all font-mono text-[11px] text-fg-strong">{String(value)}</dd>
    </div>
  );
}

interface ToolPart {
  type: string;
  toolName?: string;
  toolCallId?: string;
  state?: string;
  input?: unknown;
  output?: unknown;
  errorText?: string;
  approval?: { id?: string };
}

interface FilePart {
  type: "file";
  url?: string;
  mediaType?: string;
  filename?: string;
}

function FileAttachment({ part }: { part: FilePart }) {
  const mediaType = part.mediaType ?? inferMediaTypeFromDataUrl(part.url) ?? "application/octet-stream";
  const filename = part.filename?.trim() || mediaType;
  const url = typeof part.url === "string" ? part.url : "";
  const canRender = url.length > 0;
  const isImage = mediaType.startsWith("image/");
  const isAudio = mediaType.startsWith("audio/");
  const isVideo = mediaType.startsWith("video/");

  return (
    <div
      data-testid="preview-file-part"
      className="rounded-sm border border-line bg-soft p-2 text-xs text-fg-soft"
    >
      <div className="flex min-w-0 items-center justify-between gap-3">
        <div className="min-w-0">
          <div className="truncate font-medium text-fg-strong">{filename}</div>
          <div className="font-mono text-[10px] text-fg-faint">{mediaType}</div>
        </div>
        <span className="rounded-pill bg-muted px-2 py-0.5 font-mono text-[10px] text-fg-soft">
          {fileKindLabel(mediaType)}
        </span>
      </div>
      {canRender && isImage ? (
        <img
          src={url}
          alt={filename}
          className="mt-2 max-h-64 max-w-full rounded-sm border border-line object-contain"
        />
      ) : canRender && isAudio ? (
        <audio src={url} controls className="mt-2 w-full" />
      ) : canRender && isVideo ? (
        <video src={url} controls className="mt-2 max-h-64 w-full rounded-sm border border-line" />
      ) : (
        <div className="mt-2 break-all font-mono text-[11px] text-fg-faint">
          {url ? compactDataUrl(url) : "(no file URL)"}
        </div>
      )}
    </div>
  );
}

function ToolInvocation({ part }: { part: ToolPart }) {
  const name = part.toolName ?? part.type.replace(/^tool-/, "") ?? "tool";
  const state = part.state ?? "input-streaming";
  const tone = TOOL_STATE_TONE[state] ?? "neutral";
  const { onApprove, onDeny } = useContext(ApprovalActionsContext);
  const approvalId = part.approval?.id;
  const awaitingApproval = state === "approval-requested" && Boolean(approvalId);
  const inputRecord =
    part.input != null && typeof part.input === "object"
      ? (part.input as Record<string, unknown>)
      : undefined;
  const requestedToolName =
    name === "PermissionConfirm" && typeof inputRecord?.tool_name === "string"
      ? inputRecord.tool_name
      : name;
  return (
    <details className="rounded-sm border border-line bg-soft text-xs" open={awaitingApproval}>
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
        <StructuredPayload value={part.input} emptyLabel="(no input)" />
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
            <StructuredPayload value={part.output} emptyLabel="(no output)" />
          </>
        ) : null}
        {awaitingApproval && approvalId ? (
          <div
            data-testid="sandbox-approval"
            className="mt-3 rounded-sm border border-tone-warn/35 bg-tone-warn/10 p-3"
          >
            <div className="text-xs text-fg-strong">
              Approve <span className="font-mono">{requestedToolName}</span> execution?
            </div>
            <div className="mt-2 flex gap-2">
              <button
                type="button"
                data-testid="sandbox-approval-allow"
                onClick={() => onApprove?.(approvalId)}
                className="rounded-sm border border-tone-success/40 bg-tone-success/15 px-3 py-1.5 text-xs font-semibold text-tone-success transition hover:bg-tone-success/25"
              >
                Allow
              </button>
              <button
                type="button"
                data-testid="sandbox-approval-deny"
                onClick={() => onDeny?.(approvalId)}
                className="rounded-sm border border-tone-error/40 bg-tone-error/15 px-3 py-1.5 text-xs font-semibold text-tone-error transition hover:bg-tone-error/25"
              >
                Deny
              </button>
            </div>
          </div>
        ) : null}
      </div>
    </details>
  );
}

function StructuredPayload({
  value,
  title,
  emptyLabel = "(no value)",
}: {
  value: unknown;
  title?: string;
  emptyLabel?: string;
}) {
  if (value === undefined) {
    return <div className="mt-1 text-xs italic text-fg-faint">{emptyLabel}</div>;
  }
  const displayValue = sanitizePreviewValue(value);
  const summary = renderValueSummary(displayValue);
  const raw = formatJson(displayValue);
  const isStructured =
    displayValue !== null && typeof displayValue === "object" && !isDateLike(displayValue);

  if (!isStructured) {
    return (
      <div className="mt-1 rounded-sm bg-soft px-2 py-1.5 text-xs text-fg">
        {summary}
      </div>
    );
  }

  const entries = Array.isArray(displayValue)
    ? displayValue.slice(0, 6).map((item, index) => [String(index), item] as const)
    : Object.entries(displayValue as Record<string, unknown>).slice(0, 8);
  const total = Array.isArray(displayValue)
    ? displayValue.length
    : Object.keys(displayValue as Record<string, unknown>).length;
  const headline = Array.isArray(displayValue)
    ? `${total} item${total === 1 ? "" : "s"}`
    : `${total} field${total === 1 ? "" : "s"}`;

  return (
    <div
      data-testid={title === "Structured response" ? "structured-json-text" : undefined}
      className="mt-1 rounded-sm border border-line bg-surface p-2 text-xs text-fg"
    >
      <div className="flex flex-wrap items-center justify-between gap-2">
        <div className="font-medium text-fg-strong">{title ?? "Payload"}</div>
        <div className="font-mono text-[10px] text-fg-faint">{headline}</div>
      </div>
      {entries.length > 0 ? (
        <dl className="mt-2 grid gap-1.5">
          {entries.map(([key, item]) => (
            <div key={key} className="grid grid-cols-[minmax(5rem,0.38fr)_1fr] gap-2">
              <dt className="min-w-0 truncate font-mono text-[11px] text-fg-soft">{key}</dt>
              <dd className="min-w-0 break-words">{renderValueSummary(item)}</dd>
            </div>
          ))}
        </dl>
      ) : (
        <div className="mt-2 text-fg-faint">Empty</div>
      )}
      {total > entries.length ? (
        <div className="mt-2 text-[11px] text-fg-faint">
          {total - entries.length} more {Array.isArray(displayValue) ? "items" : "fields"}
        </div>
      ) : null}
      <details className="mt-2 rounded-sm border border-line bg-code-bg">
        <summary className="cursor-pointer px-2 py-1 text-[10px] font-medium uppercase tracking-eyebrow text-code-fg/70">
          JSON data
        </summary>
        <pre className="max-h-48 overflow-auto border-t border-line p-2 font-mono text-[11px] leading-5 text-code-fg">
          {raw}
        </pre>
      </details>
    </div>
  );
}

function renderValueSummary(value: unknown): ReactNode {
  if (value === null) return <span className="font-mono text-fg-soft">null</span>;
  if (value === undefined) return <span className="font-mono text-fg-soft">undefined</span>;
  if (typeof value === "string") {
    return <span className="whitespace-pre-wrap">{truncateText(redactSecretString(value), 500)}</span>;
  }
  if (typeof value === "number" || typeof value === "boolean") {
    return <span className="font-mono text-fg-strong">{String(value)}</span>;
  }
  if (Array.isArray(value)) {
    if (value.length === 0) return <span className="text-fg-faint">Empty list</span>;
    if (value.every((item) => item === null || ["string", "number", "boolean"].includes(typeof item))) {
      return (
        <span>
          {value
            .slice(0, 5)
            .map((item) => (item === null ? "null" : redactSecretString(String(item))))
            .join(", ")}
          {value.length > 5 ? `, +${value.length - 5} more` : ""}
        </span>
      );
    }
    return (
      <span className="text-fg-soft">
        List · {value.length} item{value.length === 1 ? "" : "s"}
      </span>
    );
  }
  if (typeof value === "object") {
    const record = value as Record<string, unknown>;
    const readable = findReadableField(record);
    if (typeof readable === "string" && readable.length > 0) {
      return <span className="whitespace-pre-wrap">{truncateText(redactSecretString(readable), 500)}</span>;
    }
    const keys = Object.keys(record);
    return (
      <span className="text-fg-soft">
        Object · {keys.length} field{keys.length === 1 ? "" : "s"}
      </span>
    );
  }
  return <span>{String(value)}</span>;
}

function findReadableField(record: Record<string, unknown>): string | null {
  for (const key of ["answer", "final", "response", "content", "message", "text", "summary", "result", "output"]) {
    const value = record[key];
    if (typeof value === "string") return value;
  }
  return null;
}

function parseStructuredText(text: string): { ok: true; value: unknown } | { ok: false } {
  const trimmed = text.trim();
  if (!(trimmed.startsWith("{") || trimmed.startsWith("["))) {
    return { ok: false };
  }
  try {
    return { ok: true, value: JSON.parse(trimmed) };
  } catch {
    return { ok: false };
  }
}

function truncateText(value: string, max: number): string {
  if (value.length <= max) return value;
  return `${value.slice(0, max)}...`;
}

function isDateLike(value: object): boolean {
  return value instanceof Date;
}

function isRuntimeDataPart(part: { type: string; data?: unknown }): part is RuntimeDataPart {
  return (
    part.type === "data-run-info" ||
    part.type === "data-inference-complete" ||
    part.type === "data-state-snapshot"
  );
}

function inferMediaTypeFromDataUrl(url: string | undefined): string | undefined {
  if (!url?.startsWith("data:")) return undefined;
  const marker = url.indexOf(";base64,");
  if (marker < 5) return undefined;
  return url.slice(5, marker);
}

function fileKindLabel(mediaType: string): string {
  if (mediaType.startsWith("image/")) return "image";
  if (mediaType.startsWith("audio/")) return "audio";
  if (mediaType.startsWith("video/")) return "video";
  if (mediaType === "application/pdf") return "pdf";
  if (mediaType.startsWith("text/")) return "text";
  return "file";
}

function isScriptedResponseText(text: string): boolean {
  return /^Scripted response to:/i.test(text.trim());
}

function asRecord(value: unknown): Record<string, unknown> {
  return value !== null && typeof value === "object" && !Array.isArray(value)
    ? (value as Record<string, unknown>)
    : {};
}

function summaryValue<T>(value: unknown, formatter: (value: T) => string): string | undefined {
  return value === undefined || value === null ? undefined : formatter(value as T);
}

function formatDuration(value: unknown): string {
  if (typeof value !== "number" || !Number.isFinite(value)) return "";
  if (value >= 1000) return `${(value / 1000).toFixed(2)}s`;
  return `${Math.round(value)}ms`;
}

function formatUsage(usage: Record<string, unknown>): string | undefined {
  const total = usage.total_tokens;
  const prompt = usage.prompt_tokens;
  const completion = usage.completion_tokens;
  if (typeof total === "number") {
    const pieces = [`${total} total`];
    if (typeof prompt === "number") pieces.push(`${prompt} prompt`);
    if (typeof completion === "number") pieces.push(`${completion} completion`);
    return pieces.join(" · ");
  }
  return undefined;
}

function validateSandboxFiles(files: File[]): string | null {
  if (files.length > MAX_SANDBOX_FILES) {
    return `Attach at most ${MAX_SANDBOX_FILES} files in one sandbox turn.`;
  }
  const total = files.reduce((sum, file) => sum + file.size, 0);
  if (total > MAX_SANDBOX_TOTAL_BYTES) {
    return `Attachments are limited to ${formatBytes(MAX_SANDBOX_TOTAL_BYTES)} per sandbox turn.`;
  }
  return null;
}

async function filesToFileParts(files: File[]): Promise<FileUIPart[]> {
  return Promise.all(
    files.map(async (file) => ({
      type: "file" as const,
      mediaType: file.type || "application/octet-stream",
      filename: file.name || undefined,
      url: await fileToDataUrl(file),
    })),
  );
}

function fileToDataUrl(file: File): Promise<string> {
  return new Promise((resolve, reject) => {
    const reader = new FileReader();
    reader.onload = (event) => resolve(String(event.target?.result ?? ""));
    reader.onerror = () => reject(reader.error ?? new Error("Failed to read attachment."));
    reader.readAsDataURL(file);
  });
}

function sanitizePreviewValue(value: unknown): unknown {
  return compactDataUrls(redactSecretsForDisplay(value));
}

function compactDataUrls(value: unknown): unknown {
  if (typeof value === "string") {
    return compactDataUrl(redactSecretString(value));
  }
  if (Array.isArray(value)) {
    return value.map(compactDataUrls);
  }
  if (value !== null && typeof value === "object") {
    return Object.fromEntries(
      Object.entries(value as Record<string, unknown>).map(([key, item]) => [
        key,
        compactDataUrls(item),
      ]),
    );
  }
  return value;
}

function compactDataUrl(value: string): string {
  if (!value.startsWith("data:") || value.length <= 160) {
    return value;
  }
  const comma = value.indexOf(",");
  const prefix = comma > 0 ? value.slice(0, comma + 1) : "data:...,";
  return `${prefix}[base64 ${formatBytes(estimateBase64Bytes(value.slice(comma + 1)))}]`;
}

function estimateBase64Bytes(data: string): number {
  const padding = data.endsWith("==") ? 2 : data.endsWith("=") ? 1 : 0;
  return Math.max(0, Math.floor((data.length * 3) / 4) - padding);
}

function formatBytes(bytes: number): string {
  if (!Number.isFinite(bytes) || bytes < 0) return "0 B";
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
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
  if (typed.type === "file") {
    return true;
  }
  if (typed.type === "dynamic-tool" || typed.type.startsWith("tool-")) {
    return true;
  }
  if (isRuntimeDataPart(typed)) {
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
  if (typeof value === "string") return compactDataUrl(redactSecretString(value));
  // R10 #5 — tool inputs/outputs can carry API keys, authorization
  // headers, cookies, JWTs etc. Same redaction pipeline used by audit /
  // trace / diff so a credential never lands in the preview DOM.
  const redacted = sanitizePreviewValue(value);
  try {
    return JSON.stringify(redacted, null, 2);
  } catch {
    return String(redacted);
  }
}

export function normalizePreviewAgent(draft: AgentSpec): AgentSpec {
  // Strip provenance / locality fields before sending to the preview
  // endpoint. The server's `/v1/ai-sdk/agent-previews/runs` route returns
  // 400 if `backend`, `endpoint`, or `registry` would make the draft non-local.
  // It forces the preview into the local-only resolve path so a crafted
  // draft can't route the run to an arbitrary remote backend or skip
  // registry-membership checks. Builtin / customized records loaded into
  // the editor naturally carry `registry` (and may carry remote backend
  // fields), so without this strip every preview of a registry-resident agent would
  // fail with `BadRequest`. The runtime preview is always local —
  // backend, endpoint, and registry are not meaningful here.
  // `String(x ?? "")` on every string field so a partial draft (missing `id` etc) doesn't crash.
  const {
    backend: _backend,
    endpoint: _endpoint,
    registry: _registry,
    ...localDraft
  } = draft;
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
