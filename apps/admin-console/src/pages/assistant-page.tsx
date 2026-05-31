import { useChat } from "@ai-sdk/react";
import { DefaultChatTransport } from "ai";
import { useMemo, useRef, useEffect, useState, type FormEvent } from "react";
import {
  adminAssistantApi,
  adminAssistantRunUrl,
  type AdminAssistantConfig,
} from "@/lib/config-api";
import { adminAuthHeaders } from "@/lib/api/http";
import { useCapabilitiesQuery } from "@/lib/query/hooks/capabilities";
import {
  describeToolCallState,
  previewPayload,
  viewMessage,
  type AssistantBlock,
  type AssistantBlockTone,
  type RuntimeDataPart,
} from "@/lib/assistant-message";

const suggestions = [
  "Create a coding agent with Bash and file tools",
  "Set up a customer support agent with permission controls",
  "Configure a research agent that delegates to sub-agents",
  "Show me all available plugins and their options",
];

export function AssistantPage() {
  return <AssistantChatPanel variant="page" />;
}

export function AssistantChatPanel({ variant = "page" }: { variant?: "page" | "floating" }) {
  const scrollRef = useRef<HTMLDivElement>(null);
  const [input, setInput] = useState("");
  const transport = useMemo(
    () =>
      new DefaultChatTransport({
        api: adminAssistantRunUrl(),
        prepareSendMessagesRequest: ({ messages }) => ({
          headers: adminAuthHeaders(),
          body: { messages },
        }),
      }),
    [],
  );

  const { messages, sendMessage, status, error } = useChat({
    transport,
  });

  useEffect(() => {
    scrollRef.current?.scrollTo(0, scrollRef.current.scrollHeight);
  }, [messages]);

  // The AI SDK reports stream-level failures via `finishReason: "error"`
  // inside the stream itself, not via the hook's `error` ref. Treat a
  // finished run whose last message is still the user prompt as an error
  // — that's the visible symptom of "agent not found" / dead-letter.
  const streamErrored =
    !!error ||
    (status === "ready" &&
      messages.length > 0 &&
      messages[messages.length - 1]?.role === "user");

  const handleSubmit = (e: FormEvent) => {
    e.preventDefault();
    if (!input.trim() || status === "streaming") return;
    sendMessage({ text: input });
    setInput("");
  };

  return (
    // The chat layout needs an explicit height for the message list
    // (`flex-1 overflow-auto`) and the bottom input bar to behave. We
    // pin to dynamic viewport height (`100dvh`) minus the topbar so
    // the input is always anchored at the visible bottom — `h-full`
    // collapses to `auto` whenever an ancestor stops propagating a
    // definite height, hiding the input below the fold.
    <div className={variant === "floating" ? "flex h-full flex-col" : "flex h-[calc(100dvh-3rem)] flex-col"}>
      <header className="border-b border-line bg-surface px-6 py-4">
        <h2 className="text-lg font-semibold text-fg-strong">
          Admin Assistant
        </h2>
        <p className="mt-1 text-sm text-fg-soft">
          Describe the agent you want to create or modify. The assistant reads
          platform capabilities, drafts AgentSpecs, and validates settings with
          locked admin-only tools.
        </p>
        <AssistantSettingsPanel compact={variant === "floating"} />
      </header>

      <div ref={scrollRef} className="flex-1 space-y-4 overflow-auto p-6">
        {messages.length === 0 && (
          <div className="mt-12 space-y-3 text-center text-fg-faint">
            <p className="text-lg">
              What kind of agent would you like to build?
            </p>
            <div className="mt-4 flex flex-wrap justify-center gap-2">
              {suggestions.map((s) => (
                <button
                  key={s}
                  type="button"
                  onClick={() => setInput(s)}
                  className="rounded-full bg-muted px-3 py-1.5 text-sm text-fg hover:bg-muted"
                >
                  {s}
                </button>
              ))}
            </div>
          </div>
        )}

        {messages.map((message) => {
          const view = viewMessage(message);
          if (view.blocks.length === 0) return null;
          const isUser = message.role === "user";
          return (
            <div
              key={message.id}
              className={`flex ${isUser ? "justify-end" : "justify-start"}`}
            >
              <div
                className={`max-w-[80%] space-y-2 rounded-lg px-4 py-2 text-sm ${
                  isUser
                    ? "bg-cyan-700 text-bg"
                    : "bg-surface text-fg shadow"
                }`}
              >
                {view.blocks.map((block) => (
                  <BlockView key={block.id} block={block} isUser={isUser} />
                ))}
              </div>
            </div>
          );
        })}

        {status === "streaming" && (
          <div className="flex justify-start">
            <div className="animate-pulse rounded-lg bg-surface px-4 py-2 text-sm text-fg-faint shadow">
              Thinking...
            </div>
          </div>
        )}

        {streamErrored && (
          <div
            role="alert"
            className="rounded-lg border border-tone-error bg-surface px-4 py-3 text-sm text-tone-error shadow"
          >
            <div className="font-medium">Assistant call failed</div>
            <div className="mt-1 text-xs text-fg-soft">
              {error?.message ||
                "The admin assistant is unavailable. Configure a model and ensure the admin assistant endpoint is enabled."}
            </div>
          </div>
        )}
      </div>

      <form
        onSubmit={handleSubmit}
        className="flex gap-3 border-t border-line bg-surface px-6 py-3"
      >
        <input
          value={input}
          onChange={(e) => setInput(e.target.value)}
          placeholder="Describe your agent or ask about config..."
          className="flex-1 rounded-lg border border-line-strong px-4 py-2 text-sm focus:outline-none focus:ring-2 focus:ring-cyan-600"
        />
        <button
          type="submit"
          disabled={status === "streaming" || !input.trim()}
          className="rounded-lg bg-cyan-700 px-4 py-2 text-sm text-bg hover:bg-cyan-600 disabled:opacity-50"
        >
          Send
        </button>
      </form>
    </div>
  );
}

function AssistantSettingsPanel({ compact = false }: { compact?: boolean }) {
  const capabilitiesQuery = useCapabilitiesQuery();
  const [config, setConfig] = useState<AdminAssistantConfig | null>(null);
  const [draft, setDraft] = useState<AdminAssistantConfig | null>(null);
  const [loading, setLoading] = useState(true);
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [saved, setSaved] = useState(false);

  useEffect(() => {
    let alive = true;
    setLoading(true);
    adminAssistantApi
      .getConfig()
      .then((next) => {
        if (!alive) return;
        setConfig(next);
        setDraft(next);
        setError(null);
      })
      .catch((err) => {
        if (!alive) return;
        setError(errorMessage(err));
      })
      .finally(() => {
        if (alive) setLoading(false);
      });
    return () => {
      alive = false;
    };
  }, []);

  async function save() {
    if (!draft) return;
    setSaving(true);
    setSaved(false);
    try {
      const next = await adminAssistantApi.updateConfig(draft);
      setConfig(next);
      setDraft(next);
      setError(null);
      setSaved(true);
    } catch (err) {
      setError(errorMessage(err));
    } finally {
      setSaving(false);
    }
  }

  const dirty = JSON.stringify(config) !== JSON.stringify(draft);
  const models = capabilitiesQuery.data?.models ?? [];

  return (
    <details className="mt-4 rounded-sm border border-line bg-soft">
      <summary className="cursor-pointer px-3 py-2 text-xs font-medium text-fg-soft">
        Prompt policy and locked tools
      </summary>
      <div className="space-y-3 border-t border-line px-3 py-3">
        <div className={compact ? "grid gap-3" : "grid gap-3 md:grid-cols-[16rem,1fr]"}>
          <label className="text-xs font-medium text-fg-soft">
            Assistant model
            <select
              value={draft?.model_id ?? ""}
              onChange={(event) =>
                setDraft((current) =>
                  current
                    ? {
                        ...current,
                        model_id: event.target.value.trim() || null,
                      }
                    : current,
                )
              }
              disabled={!draft || loading || saving}
              className="mt-1 w-full rounded-sm border border-line-strong bg-surface px-2 py-1.5 text-sm text-fg outline-none focus:border-fg"
            >
              <option value="">Auto-select first configured model</option>
              {models.map((model) => (
                <option key={model.id} value={model.id}>
                  {model.id}
                </option>
              ))}
            </select>
          </label>
          <label className="text-xs font-medium text-fg-soft">
            Editable policy prompt
            <textarea
              value={draft?.policy_prompt ?? ""}
              onChange={(event) =>
                setDraft((current) =>
                  current ? { ...current, policy_prompt: event.target.value } : current,
                )
              }
              disabled={!draft || loading || saving}
              rows={compact ? 3 : 5}
              placeholder="Organization-specific preferences for how the Admin Assistant should draft agents."
              className="mt-1 w-full rounded-sm border border-line-strong bg-surface px-3 py-2 font-mono text-xs text-fg outline-none focus:border-fg"
            />
          </label>
        </div>
        <div className="flex flex-wrap items-center gap-2 text-xs text-fg-soft">
          <span className="rounded-pill bg-muted px-2 py-0.5">system prompt locked</span>
          <span className="rounded-pill bg-muted px-2 py-0.5">tools locked</span>
          <span className="rounded-pill bg-muted px-2 py-0.5">admin only</span>
          {error ? <span className="text-tone-error">{error}</span> : null}
          {saved ? <span className="text-tone-success">Saved</span> : null}
          <button
            type="button"
            onClick={() => void save()}
            disabled={!dirty || saving || loading || !draft}
            className="ml-auto rounded-sm border border-line-strong bg-surface px-2 py-1 text-xs font-medium text-fg transition hover:bg-muted disabled:cursor-not-allowed disabled:opacity-60"
          >
            {saving ? "Saving..." : "Save policy"}
          </button>
        </div>
      </div>
    </details>
  );
}

function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

function BlockView({ block, isUser }: { block: AssistantBlock; isUser: boolean }) {
  switch (block.kind) {
    case "text":
      return <div className="whitespace-pre-wrap break-words">{block.text}</div>;
    case "reasoning":
      return (
        <div
          className={[
            "rounded-sm px-3 py-2 text-xs italic leading-5",
            isUser ? "bg-cyan-800 text-cyan-50" : "bg-soft text-fg-soft",
          ].join(" ")}
        >
          <div className="text-[10px] font-semibold uppercase tracking-wide opacity-70">
            Reasoning
          </div>
          <div className="mt-1 whitespace-pre-wrap break-words">{block.text}</div>
        </div>
      );
    case "step-start":
      return (
        <div className="text-[10px] font-semibold uppercase tracking-wide text-fg-faint">
          ── new step ──
        </div>
      );
    case "runtime-metadata":
      return <RuntimeMetadataView parts={block.parts} />;
    case "tool-call":
      return <ToolCallView block={block} />;
    case "unknown":
      return (
        <div
          className={[
            "rounded-sm px-2 py-1 text-[11px] font-mono",
            isUser ? "bg-cyan-800 text-cyan-100" : "bg-muted text-fg-soft",
          ].join(" ")}
        >
          [unrendered: {block.type || "unknown"}]
        </div>
      );
  }
}

function RuntimeMetadataView({ parts }: { parts: RuntimeDataPart[] }) {
  const runInfo = parts.find((part) => part.type === "data-run-info");
  const inference = [...parts].reverse().find((part) => part.type === "data-inference-complete");
  const snapshots = parts.filter((part) => part.type === "data-state-snapshot");
  const latestState = snapshots.at(-1);
  const inferenceData = asRecord(inference?.data);
  const usage = asRecord(inferenceData.usage);
  const lifecycle = asRecord(
    asRecord(asRecord(latestState?.data).extensions)?.["__runtime.run_lifecycle"],
  );
  const summary = [
    typeof lifecycle.status === "string" ? lifecycle.status : null,
    typeof inferenceData.model === "string" ? inferenceData.model : null,
    typeof usage.total_tokens === "number" ? `${usage.total_tokens} tokens` : null,
    typeof inferenceData.durationMs === "number" ? formatDuration(inferenceData.durationMs) : null,
  ].filter(Boolean);

  return (
    <details className="rounded-sm border border-line bg-soft px-3 py-2 text-xs text-fg-soft">
      <summary className="cursor-pointer text-[10px] font-semibold uppercase tracking-wide text-fg-soft">
        Runtime metadata{summary.length > 0 ? ` · ${summary.join(" · ")}` : ""}
      </summary>
      <dl className="mt-2 grid gap-1.5">
        <MetadataRow label="Run" value={asRecord(runInfo?.data).runId} />
        <MetadataRow label="Thread" value={asRecord(runInfo?.data).threadId} />
        <MetadataRow label="Model" value={inferenceData.model} />
        <MetadataRow label="Duration" value={summaryValue(inferenceData.durationMs, formatDuration)} />
        <MetadataRow label="Usage" value={formatUsage(usage)} />
        <MetadataRow label="State snapshots" value={snapshots.length} />
      </dl>
      <details className="mt-2 rounded-sm border border-line bg-code-bg">
        <summary className="cursor-pointer px-2 py-1 text-[10px] font-medium uppercase tracking-wide text-code-fg/70">
          JSON data
        </summary>
        <pre className="max-h-48 overflow-auto border-t border-line p-2 font-mono text-[11px] leading-5 text-code-fg">
          {previewPayload(parts)}
        </pre>
      </details>
    </details>
  );
}

function MetadataRow({ label, value }: { label: string; value: unknown }) {
  if (value === undefined || value === null || value === "") return null;
  return (
    <div className="grid grid-cols-[5rem_1fr] gap-2">
      <dt className="text-fg-soft">{label}</dt>
      <dd className="min-w-0 break-all font-mono text-[11px] text-fg-strong">{String(value)}</dd>
    </div>
  );
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

const TONE_CLASS: Record<AssistantBlockTone, string> = {
  info: "bg-muted text-fg",
  success: "bg-tone-success/15 text-tone-success",
  warn: "bg-tone-warn/15 text-tone-warn",
  error: "bg-tone-error/15 text-tone-error",
};

function ToolCallView({
  block,
}: {
  block: Extract<AssistantBlock, { kind: "tool-call" }>;
}) {
  const description = describeToolCallState(block.state);
  const inputPreview = previewPayload(block.input);
  const outputPreview = previewPayload(block.output);
  return (
    <details className="group rounded-sm border border-line bg-soft text-fg open:bg-surface">
      <summary className="flex cursor-pointer flex-wrap items-center gap-2 px-3 py-2 text-xs">
        <span className="font-semibold">🛠</span>
        <span className="font-mono">{block.toolName}</span>
        <span
          className={[
            "rounded-full px-2 py-0.5 text-[10px] font-semibold uppercase tracking-wide",
            TONE_CLASS[description.tone],
          ].join(" ")}
        >
          {description.label}
        </span>
        <span className="ml-auto text-[10px] text-fg-faint group-open:hidden">
          click to expand
        </span>
      </summary>
      <div className="space-y-2 border-t border-line px-3 py-2">
        {inputPreview ? (
          <PayloadPanel label="Input" payload={inputPreview} />
        ) : null}
        {outputPreview ? (
          <PayloadPanel label="Output" payload={outputPreview} />
        ) : null}
        {block.errorText ? (
          <PayloadPanel label="Error" payload={block.errorText} tone="error" />
        ) : null}
      </div>
    </details>
  );
}

function PayloadPanel({
  label,
  payload,
  tone = "info",
}: {
  label: string;
  payload: string;
  tone?: AssistantBlockTone;
}) {
  return (
    <div>
      <div
        className={[
          "inline-block rounded-full px-2 py-0.5 text-[10px] font-semibold uppercase tracking-wide",
          TONE_CLASS[tone],
        ].join(" ")}
      >
        {label}
      </div>
      <pre className="mt-1 max-h-72 overflow-auto rounded-sm bg-code-bg px-3 py-2 text-[11px] leading-5 text-code-fg">
        {payload}
      </pre>
    </div>
  );
}
