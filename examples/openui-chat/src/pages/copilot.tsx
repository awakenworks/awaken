import { Copilot } from "@openuidev/react-ui";
import { agUIAdapter } from "@openuidev/react-headless";
import { AGENT_ID, agUiRunUrl } from "@/lib/config";
import {
  fetchThreadList,
  loadThread,
  createThread,
  deleteThread,
  updateThread,
} from "@/lib/thread-api";

function MockApp() {
  return (
    <div className="flex h-full flex-col items-center justify-center bg-slate-50 p-8">
      <h1 className="mb-4 text-2xl font-bold text-slate-800">
        Your Application
      </h1>
      <p className="max-w-md text-center text-slate-600">
        This area represents your main application content. The Copilot sidebar
        is available on the right side for AI assistance.
      </p>
    </div>
  );
}

export function CopilotPage() {
  return (
    <div className="flex h-[calc(100vh-2.5rem)]">
      <main className="flex-1">
        <MockApp />
      </main>
      <Copilot
        apiUrl={agUiRunUrl(AGENT_ID)}
        streamProtocol={agUIAdapter()}
        agentName="Awaken Assistant"
        fetchThreadList={fetchThreadList}
        loadThread={loadThread}
        createThread={createThread}
        deleteThread={deleteThread}
        updateThread={updateThread}
        theme={{ mode: "light" }}
      />
    </div>
  );
}
