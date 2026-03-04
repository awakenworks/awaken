import { useChatSession } from "@/hooks/use-chat-session";
import { MessageList } from "./message-list";
import { ChatInput } from "./chat-input";
import { MetricsPanel } from "./metrics-panel";
import { ToolProgressPanel } from "./tool-progress-panel";
import type { StarterAction } from "@/lib/recommended-actions";
import { useEffect, useState } from "react";

type ChatPanelProps = {
  threadId: string;
  agentId?: string;
  themeMode?: "light" | "dark";
  layout?: "default" | "conversation";
  recommendedActions?: StarterAction[];
};

export function ChatPanel({
  threadId,
  agentId = "default",
  themeMode = "light",
  layout = "default",
  recommendedActions = [],
}: ChatPanelProps) {
  const [frontendBgColor, setFrontendBgColor] = useState<string | null>(null);
  const {
    messages,
    sendMessage,
    status,
    error,
    addToolOutput,
    historyLoaded,
    metrics,
    toolProgress,
    askAnswers,
    setAskAnswers,
  } = useChatSession(threadId, agentId);

  const isLoading = status === "streaming" || status === "submitted";
  const showQuickActions = recommendedActions.length > 0;

  useEffect(() => {
    setFrontendBgColor(null);
  }, [threadId]);

  if (!historyLoaded) {
    const loadingClass =
      themeMode === "dark" ? "text-slate-400" : "text-slate-400";
    return (
      <div className={`flex h-full items-center justify-center text-sm ${loadingClass}`}>
        Loading...
      </div>
    );
  }

  return (
    <div className="flex h-full flex-col">
      {showQuickActions && (
        <div
          className={
            themeMode === "dark"
              ? "border-b border-slate-700 bg-slate-900/70 px-4 py-3"
              : "border-b border-slate-200 bg-slate-50 px-4 py-3"
          }
        >
          <div className={themeMode === "dark" ? "text-xs font-semibold text-slate-300" : "text-xs font-semibold text-slate-600"}>
            Recommended Actions
          </div>
          <div className="mt-2 flex flex-wrap gap-2">
            {recommendedActions.map((action) => (
              <button
                key={action.id}
                type="button"
                disabled={isLoading}
                onClick={() => sendMessage({ text: action.prompt })}
                className={
                  themeMode === "dark"
                    ? "rounded-full border border-slate-600 bg-slate-800 px-3 py-1.5 text-xs font-medium text-slate-200 transition hover:bg-slate-700 disabled:opacity-50"
                    : "rounded-full border border-slate-300 bg-white px-3 py-1.5 text-xs font-medium text-slate-700 transition hover:bg-slate-100 disabled:opacity-50"
                }
              >
                {action.title}
              </button>
            ))}
          </div>
        </div>
      )}
      <div
        data-testid="chat-frontend-surface"
        className="min-h-0 flex-1"
        style={
          frontendBgColor
            ? {
                backgroundImage: `linear-gradient(180deg, ${frontendBgColor}22 0%, transparent 36%)`,
              }
            : undefined
        }
      >
        <MessageList
          messages={messages}
          isLoading={isLoading}
          askAnswers={askAnswers}
          onAskAnswerChange={(toolCallId, value) =>
            setAskAnswers((prev) => ({ ...prev, [toolCallId]: value }))
          }
          onApprove={async (id) => {
            await addToolOutput({
              tool: "PermissionConfirm" as never,
              toolCallId: id,
              state: "output-available",
              output: { approved: true } as never,
            });
          }}
          onDeny={async (id) => {
            await addToolOutput({
              tool: "PermissionConfirm" as never,
              toolCallId: id,
              state: "output-denied",
              output: { approved: false } as never,
            });
          }}
          onAskSubmit={async (toolCallId, answer) => {
            await addToolOutput({
              tool: "askUserQuestion" as never,
              toolCallId,
              state: "output-available",
              output: { message: answer } as never,
            });
            setAskAnswers((prev) => ({ ...prev, [toolCallId]: "" }));
          }}
          onFrontendToolSubmit={async (toolCallId, toolName, output) => {
            if (
              toolName === "set_background_color" &&
              typeof output.color === "string"
            ) {
              setFrontendBgColor(output.color);
            }
            await addToolOutput({
              tool: toolName as never,
              toolCallId,
              state: "output-available",
              output: output as never,
            });
          }}
          themeMode={themeMode}
          layout={layout}
        />
      </div>
      {error && (
        <div
          className={
            themeMode === "dark"
              ? "bg-red-950/30 px-4 py-2 text-sm text-red-300"
              : "bg-red-50 px-4 py-2 text-sm text-red-600"
          }
        >
          Error: {error.message}
        </div>
      )}
      <ToolProgressPanel progressByNodeId={toolProgress} />
      <MetricsPanel metrics={metrics} />
      <ChatInput
        onSend={(text) => sendMessage({ text })}
        disabled={isLoading}
        themeMode={themeMode}
      />
    </div>
  );
}
