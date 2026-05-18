import { useChat } from "@ai-sdk/react";
import { DefaultChatTransport } from "ai";
import { useRef, useEffect, useState, type FormEvent } from "react";
import { BACKEND_URL } from "@/lib/config-api";
import {
  describeToolCallState,
  previewPayload,
  viewMessage,
  type AssistantBlock,
  type AssistantBlockTone,
} from "@/lib/assistant-message";

const suggestions = [
  "Create a coding agent with Bash and file tools",
  "Set up a customer support agent with permission controls",
  "Configure a research agent that delegates to sub-agents",
  "Show me all available plugins and their options",
];

/**
 * AI Config Assistant — chat interface for AI-driven agent configuration.
 *
 * Uses a dedicated "config-assistant" agent that has tools to list, create,
 * update agents/models/providers, read capabilities, and validate configs.
 */
export function AssistantPage() {
  const scrollRef = useRef<HTMLDivElement>(null);
  const [input, setInput] = useState("");

  const { messages, sendMessage, status } = useChat({
    transport: new DefaultChatTransport({
      api: `${BACKEND_URL}/v1/ai-sdk/agents/config-assistant/runs`,
    }),
  });

  useEffect(() => {
    scrollRef.current?.scrollTo(0, scrollRef.current.scrollHeight);
  }, [messages]);

  const handleSubmit = (e: FormEvent) => {
    e.preventDefault();
    if (!input.trim() || status === "streaming") return;
    sendMessage({ text: input });
    setInput("");
  };

  return (
    <div className="flex h-full flex-col">
      <header className="border-b border-line bg-surface px-6 py-4">
        <h2 className="text-lg font-semibold text-fg-strong">
          AI Config Assistant
        </h2>
        <p className="mt-1 text-sm text-fg-soft">
          Describe the agent you want to create or modify. The assistant will
          generate configurations, suggest plugins, and validate settings.
        </p>
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
