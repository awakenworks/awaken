import { useChat } from "@ai-sdk/react";
import { DefaultChatTransport } from "ai";
import { useRef, useEffect, useState, type FormEvent } from "react";
import { BACKEND_URL } from "@/lib/config-api";

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

  const getMessageText = (msg: (typeof messages)[number]): string => {
    if (!msg.parts) return "";
    return msg.parts
      .filter(
        (p): p is { type: "text"; text: string } =>
          (p as { type: string }).type === "text",
      )
      .map((p) => p.text)
      .join("");
  };

  return (
    <div className="flex h-full flex-col">
      <header className="border-b border-slate-200 bg-white px-6 py-4">
        <h2 className="text-lg font-semibold text-slate-900">
          AI Config Assistant
        </h2>
        <p className="mt-1 text-sm text-slate-500">
          Describe the agent you want to create or modify. The assistant will
          generate configurations, suggest plugins, and validate settings.
        </p>
      </header>

      <div ref={scrollRef} className="flex-1 space-y-4 overflow-auto p-6">
        {messages.length === 0 && (
          <div className="mt-12 space-y-3 text-center text-slate-400">
            <p className="text-lg">
              What kind of agent would you like to build?
            </p>
            <div className="mt-4 flex flex-wrap justify-center gap-2">
              {suggestions.map((s) => (
                <button
                  key={s}
                  type="button"
                  onClick={() => setInput(s)}
                  className="rounded-full bg-slate-100 px-3 py-1.5 text-sm text-slate-700 hover:bg-slate-200"
                >
                  {s}
                </button>
              ))}
            </div>
          </div>
        )}

        {messages.map((m) => {
          const text = getMessageText(m);
          if (!text) return null;
          return (
            <div
              key={m.id}
              className={`flex ${m.role === "user" ? "justify-end" : "justify-start"}`}
            >
              <div
                className={`max-w-[80%] whitespace-pre-wrap rounded-lg px-4 py-2 text-sm ${
                  m.role === "user"
                    ? "bg-cyan-700 text-white"
                    : "bg-white text-slate-800 shadow"
                }`}
              >
                {text}
              </div>
            </div>
          );
        })}

        {status === "streaming" && (
          <div className="flex justify-start">
            <div className="animate-pulse rounded-lg bg-white px-4 py-2 text-sm text-slate-400 shadow">
              Thinking...
            </div>
          </div>
        )}
      </div>

      <form
        onSubmit={handleSubmit}
        className="flex gap-3 border-t border-slate-200 bg-white px-6 py-3"
      >
        <input
          value={input}
          onChange={(e) => setInput(e.target.value)}
          placeholder="Describe your agent or ask about config..."
          className="flex-1 rounded-lg border border-slate-300 px-4 py-2 text-sm focus:outline-none focus:ring-2 focus:ring-cyan-600"
        />
        <button
          type="submit"
          disabled={status === "streaming" || !input.trim()}
          className="rounded-lg bg-cyan-700 px-4 py-2 text-sm text-white hover:bg-cyan-600 disabled:opacity-50"
        >
          Send
        </button>
      </form>
    </div>
  );
}
