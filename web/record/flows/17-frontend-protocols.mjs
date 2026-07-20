// Frontend protocol proof: AI SDK and AG-UI drive the same published Agent and
// durable thread, then each adapter exposes the shared committed history.
import { LIVE_MODEL_ID, configureLiveModel } from "../support/models.mjs";

const AGENT_ID = "frontend-protocol-agent";
const THREAD_ID = `protocol-thread-${Date.now()}`;

export const story = {
  promise: "Use the frontend framework protocol your application already speaks without creating a second Agent implementation.",
  effect: "AI SDK and AG-UI both drive the same published Agent and commit into one durable thread history.",
  aha: "Two frontend protocols become two views of one Agent thread—not two integration silos.",
  loyalty: "Framework changes no longer force teams to migrate Agent definitions or lose operational history.",
  satisfaction: "Built-in endpoints and shared history make protocol diagnosis an inspectable, repeatable check.",
  advocacy: "The same thread crossing AI SDK and AG-UI is a compact interoperability proof developers can reproduce.",
};

export async function run({ page, goto, intro, beat, clearCaption, runtimeCheckpoint, aha, expect, wait }) {
  await configureLiveModel(page);
  await page.request.put(`http://127.0.0.1:38080/v1/config/agents/${AGENT_ID}`, {
    data: {
      id: AGENT_ID, name: "Frontend protocol agent", model: { id: LIVE_MODEL_ID },
      system: "Reply with PROTOCOL READY and a short phrase naming the user's request.",
      tools: [], mcp_servers: [], skills: [], plugins: [], plugin_config: {},
      context_policy: { kind: "keep_all" }, max_steps: 4,
    },
  });
  const published = await page.request.post(`http://127.0.0.1:38080/v1/config/agents/${AGENT_ID}/publish`);
  expect(published.ok()).toBeTruthy();

  await goto("/w/default/protocols");
  await intro(
    "Connect AI SDK and AG-UI applications without duplicating the Agent or splitting its history.",
    "Every frontend adapter drives the same protocol-neutral runtime and commits to the same durable thread.",
  );
  await beat("AI SDK streams the first request through its native data-stream endpoint.", page.getByText("Vercel AI SDK", { exact: true }), 3400);
  const ai = await page.request.post(`http://127.0.0.1:38080/v1/ai-sdk/agents/${AGENT_ID}/runs`, {
    data: {
      threadId: THREAD_ID,
      messages: [{ id: "u-ai", role: "user", parts: [{ type: "text", text: "from AI SDK" }] }],
    },
  });
  expect(ai.ok()).toBeTruthy();
  expect(await ai.text()).toContain("PROTOCOL READY");

  await beat("AG-UI continues the same thread and receives its own native SSE event stream.", page.getByText("AG-UI", { exact: true }), 3400);
  const ag = await page.request.post(`http://127.0.0.1:38080/v1/ag-ui/agents/${AGENT_ID}`, {
    data: {
      threadId: THREAD_ID,
      runId: "ag-run-1",
      messages: [{ id: "u-ag", role: "user", content: "from AG-UI" }],
    },
  });
  expect(ag.ok()).toBeTruthy();
  const agStream = await ag.text();
  expect(agStream).toContain("RUN_STARTED");
  expect(agStream).toContain("PROTOCOL READY");
  expect(agStream).toContain("RUN_FINISHED");

  await runtimeCheckpoint("AI SDK and AG-UI expose one shared committed thread", async () => {
    const aiHistory = await page.request.get(`http://127.0.0.1:38080/v1/ai-sdk/threads/${THREAD_ID}/messages`);
    const agHistory = await page.request.get(`http://127.0.0.1:38080/v1/ag-ui/threads/${THREAD_ID}/messages`);
    expect(aiHistory.ok()).toBeTruthy();
    expect(agHistory.ok()).toBeTruthy();
    const aiBody = await aiHistory.json();
    const agBody = await agHistory.json();
    expect(JSON.stringify(aiBody)).toContain("from AI SDK");
    expect(JSON.stringify(aiBody)).toContain("from AG-UI");
    expect(JSON.stringify(agBody)).toContain("from AI SDK");
    expect(JSON.stringify(agBody)).toContain("from AG-UI");
  });
  await clearCaption();
  await aha(story.aha);
  await wait(900);
  await clearCaption();
}
