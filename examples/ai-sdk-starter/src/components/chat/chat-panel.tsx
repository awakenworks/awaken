import { useChatSession } from "@/hooks/use-chat-session";
import { MessageList } from "./message-list";
import { ChatInput } from "./chat-input";
import { MetricsPanel } from "./metrics-panel";
import { ToolProgressPanel } from "./tool-progress-panel";
import type { StarterAction } from "@/lib/recommended-actions";
import { useEffect, useRef, useState } from "react";

type ChatPanelProps = {
  threadId: string | null;
  agentId?: string;
  themeMode?: "light" | "dark";
  layout?: "default" | "conversation";
  recommendedActions?: StarterAction[];
  onRequestThread?: () => string;
  onFirstMessage?: (threadId: string, text: string) => void;
};

export function ChatPanel({
  threadId,
  agentId = "default",
  themeMode = "light",
  layout = "conversation",
  recommendedActions = [],
  onRequestThread,
  onFirstMessage,
}: ChatPanelProps) {
  const [frontendBgColor, setFrontendBgColor] = useState<string | null>(null);
  const [resolvedThreadId, setResolvedThreadId] = useState(threadId);
  const pendingRef = useRef<string | null>(null);
  const hasSentFirst = useRef(false);

  useEffect(() => {
    setResolvedThreadId(threadId);
    hasSentFirst.current = false;
    setFrontendBgColor(null);
  }, [threadId]);

  // Draft mode: no thread yet
  if (!resolvedThreadId) {
    const handleDraftSend = (text: string) => {
      if (onRequestThread) {
        const newId = onRequestThread();
        pendingRef.current = text;
        hasSentFirst.current = false;
        setResolvedThreadId(newId);
        onFirstMessage?.(newId, text);
      }
    };

    return (
      <div className="flex h-full flex-col">
        <WelcomeScreen
          actions={recommendedActions}
          agentId={agentId}
          onSend={handleDraftSend}
        />
        <ChatInput onSend={handleDraftSend} disabled={false} themeMode={themeMode} />
      </div>
    );
  }

  return (
    <ActiveChatPanel
      key={resolvedThreadId}
      threadId={resolvedThreadId}
      agentId={agentId}
      themeMode={themeMode}
      layout={layout}
      recommendedActions={recommendedActions}
      frontendBgColor={frontendBgColor}
      setFrontendBgColor={setFrontendBgColor}
      pendingRef={pendingRef}
      hasSentFirst={hasSentFirst}
      onFirstMessage={onFirstMessage}
    />
  );
}

function WelcomeScreen({
  actions,
  agentId,
  onSend,
}: {
  actions: StarterAction[];
  agentId: string;
  onSend: (text: string) => void;
}) {
  return (
    <div className="flex flex-1 flex-col items-center justify-center px-4">
      <h1 className="text-2xl font-semibold text-slate-800">awaken assistant</h1>
      <p className="mt-2 text-sm text-slate-500">
        Start a conversation or pick an action below.
      </p>
      {actions.length > 0 && (
        <div className="mt-6 flex max-w-2xl flex-wrap justify-center gap-2">
          {actions.map((action) => (
            <button
              key={action.id}
              type="button"
              data-testid={`scenario-action-${action.scenarioId}`}
              onClick={() => onSend(action.prompt)}
              className="rounded-lg border border-slate-200 bg-white px-3 py-2 text-left text-sm text-slate-700 shadow-sm transition hover:border-slate-300 hover:shadow"
            >
              <div className="font-medium">{action.title}</div>
              <div className="mt-0.5 text-xs text-slate-400">{action.capability}</div>
            </button>
          ))}
        </div>
      )}
      {actions.length === 0 && (
        <p className="mt-4 text-sm text-slate-400">
          No recommended actions for the current agent. Type a message to begin.
        </p>
      )}
    </div>
  );
}

function ActiveChatPanel({
  threadId,
  agentId,
  themeMode,
  layout,
  recommendedActions,
  frontendBgColor,
  setFrontendBgColor,
  pendingRef,
  hasSentFirst,
  onFirstMessage,
}: {
  threadId: string;
  agentId: string;
  themeMode: "light" | "dark";
  layout: "default" | "conversation";
  recommendedActions: StarterAction[];
  frontendBgColor: string | null;
  setFrontendBgColor: (c: string | null) => void;
  pendingRef: React.MutableRefObject<string | null>;
  hasSentFirst: React.MutableRefObject<boolean>;
  onFirstMessage?: (threadId: string, text: string) => void;
}) {
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
    generativeUI,
  } = useChatSession(threadId, agentId);

  const isLoading = status === "streaming" || status === "submitted";

  // Send pending message once chat session is loaded
  useEffect(() => {
    if (historyLoaded && pendingRef.current) {
      const text = pendingRef.current;
      pendingRef.current = null;
      sendMessage({ text });
    }
  }, [historyLoaded, sendMessage, pendingRef]);

  const handleSend = (text: string) => {
    if (!hasSentFirst.current) {
      hasSentFirst.current = true;
      onFirstMessage?.(threadId, text);
    }
    sendMessage({ text });
  };

  if (!historyLoaded) {
    return (
      <div className="flex h-full items-center justify-center text-sm text-slate-400">
        Loading...
      </div>
    );
  }

  const showWelcome = messages.length === 0 && recommendedActions.length > 0;

  return (
    <div className="flex h-full flex-col">
      {showWelcome && (
        <WelcomeScreen
          actions={recommendedActions}
          agentId={agentId}
          onSend={handleSend}
        />
      )}
      {!showWelcome && (
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
          <div className="flex h-full min-h-0 flex-col">
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
              onSendMessage={(text) => handleSend(text)}
              themeMode={themeMode}
              layout={layout}
              generativeUI={generativeUI}
            />
          </div>
        </div>
      )}
      {error && (
        <div className="bg-red-50 px-4 py-2 text-sm text-red-600">
          Error: {error.message}
        </div>
      )}
      <ToolProgressPanel progressByNodeId={toolProgress} />
      <MetricsPanel metrics={metrics} />
      <ChatInput
        onSend={handleSend}
        disabled={isLoading}
        themeMode={themeMode}
      />
    </div>
  );
}
