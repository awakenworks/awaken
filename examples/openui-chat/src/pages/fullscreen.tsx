import { FullScreen } from "@openuidev/react-ui";
import { openuiChatLibrary } from "@openuidev/react-ui";
import { AGENT_ID, agUiRunUrl } from "@/lib/config";
import { awakenAgUiAdapter } from "@/lib/ag-ui-adapter";
import {
  fetchThreadList,
  loadThread,
  createThread,
  deleteThread,
  updateThread,
} from "@/lib/thread-api";

const conversationStarters = {
  variant: "short" as const,
  options: [
    { displayText: "What can you do?", prompt: "What can you do?" },
    { displayText: "Show me a weather card", prompt: "Show me a weather card for Beijing" },
    { displayText: "Create a todo list", prompt: "Create a todo list UI" },
  ],
};

const welcomeMessage = {
  title: "Welcome to Awaken",
  description: "Ask me anything or pick a starter below.",
};

export function FullScreenPage() {
  return (
    <div className="h-[calc(100vh-2.5rem)]">
      <FullScreen
        apiUrl={agUiRunUrl(AGENT_ID)}
        streamProtocol={awakenAgUiAdapter()}
        componentLibrary={openuiChatLibrary}
        agentName="Awaken Assistant"
        fetchThreadList={fetchThreadList}
        loadThread={loadThread}
        createThread={createThread}
        deleteThread={deleteThread}
        updateThread={updateThread}
        welcomeMessage={welcomeMessage}
        conversationStarters={conversationStarters}
        theme={{ mode: "light" }}
      />
    </div>
  );
}
