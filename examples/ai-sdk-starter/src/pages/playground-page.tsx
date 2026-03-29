import { useSearchParams } from "react-router";
import { ChatLayout } from "@/components/layout/chat-layout";
import { ChatPanel } from "@/components/chat/chat-panel";
import { RECOMMENDED_ACTIONS } from "@/lib/recommended-actions";
import { useThreads } from "@/hooks/use-threads";

const AGENT_OPTIONS = [
  {
    id: "default",
    title: "Default Agent",
    desc: "Backend tools + ask-user + frontend tool interaction",
  },
  {
    id: "permission",
    title: "Permission Agent",
    desc: "PermissionConfirm flow with one-click approve/deny",
  },
{
    id: "a2ui",
    title: "A2UI Agent",
    desc: "Declarative UI via A2UI protocol (render_a2ui tool)",
  },
] as const;

export function PlaygroundPage() {
  const [searchParams, setSearchParams] = useSearchParams();
  const {
    threads,
    activeThreadId,
    setActiveThreadId,
    startNewChat,
    createThread,
    renameThread,
    removeThread,
    autoTitle,
    loaded,
  } = useThreads();

  const agentId = searchParams.get("agentId")?.trim() || "default";
  const activeAgent =
    AGENT_OPTIONS.find((item) => item.id === agentId) ?? AGENT_OPTIONS[0];
  const recommendedActions = RECOMMENDED_ACTIONS.filter(
    (action) => !action.agentId || action.agentId === activeAgent.id,
  );

  const handleAgentChange = (id: string) => {
    const next = new URLSearchParams(searchParams);
    next.set("agentId", id);
    setSearchParams(next);
    startNewChat();
  };

  // Use a stable key ("chat") so the ChatPanel instance (and its pendingRef)
  // survives the draft→active transition when createThread sets activeThreadId.
  // ActiveChatPanel is separately keyed by threadId for proper cleanup.
  const chat =
    !loaded ? (
      <div className="flex h-full items-center justify-center text-sm text-slate-400">
        Loading threads...
      </div>
    ) : (
      <ChatPanel
        threadId={activeThreadId}
        agentId={agentId}
        recommendedActions={recommendedActions}
        onRequestThread={!activeThreadId ? createThread : undefined}
        onFirstMessage={(threadId, text) => autoTitle(threadId, text)}
      />
    );

  return (
    <ChatLayout
      threads={threads}
      activeThreadId={activeThreadId}
      onSelectThread={setActiveThreadId}
      onNewChat={startNewChat}
      onRenameThread={renameThread}
      onDeleteThread={removeThread}
      agentOptions={AGENT_OPTIONS}
      activeAgentId={agentId}
      onAgentChange={handleAgentChange}
    >
      {chat}
    </ChatLayout>
  );
}
