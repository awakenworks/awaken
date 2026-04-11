import { BottomTray } from "@openuidev/react-ui";
import { awakenAgUiAdapter } from "@/lib/ag-ui-adapter";
import { AGENT_ID, agUiRunUrl } from "@/lib/config";
import {
  fetchThreadList,
  loadThread,
  createThread,
  deleteThread,
  updateThread,
} from "@/lib/thread-api";

function MockPage() {
  return (
    <div className="mx-auto max-w-3xl p-8">
      <h1 className="mb-6 text-3xl font-bold text-slate-800">Support Page</h1>
      <p className="mb-4 text-slate-600">
        This page demonstrates the BottomTray layout — a floating chat widget in
        the bottom-right corner, ideal for customer support or help desk
        experiences.
      </p>
      <p className="text-slate-600">
        Click the chat button in the bottom-right corner to start a
        conversation.
      </p>
    </div>
  );
}

export function BottomTrayPage() {
  return (
    <div className="min-h-[calc(100vh-2.5rem)] bg-white">
      <MockPage />
      <BottomTray
        apiUrl={agUiRunUrl(AGENT_ID)}
        streamProtocol={awakenAgUiAdapter()}
        agentName="Awaken Support"
        fetchThreadList={fetchThreadList}
        loadThread={loadThread}
        createThread={createThread}
        deleteThread={deleteThread}
        updateThread={updateThread}
        theme={{ mode: "light" }}
        defaultOpen={false}
      />
    </div>
  );
}
