// Frontend protocol proof: AI SDK and AG-UI drive the same published Agent and
// durable thread, then each adapter exposes the shared committed history.
import { LIVE_MODEL_ID, configureLiveModel } from "../support/models.mjs";
import { MANAGED_HEADERS } from "../support/betas.mjs";
import { BACKEND, createManagedSession, publishAgent, putAgent, requireJson, requireOk } from "../support/control-plane.mjs";

const AGENT_ID = "frontend-protocol-agent";
const THREAD_ID = `protocol-thread-${Date.now()}`;

function streamedText(body) {
  return body.split("\n")
    .filter((line) => line.startsWith("data: "))
    .flatMap((line) => {
      try {
        const event = JSON.parse(line.slice(6));
        return [event.delta, event.content, event.text].filter((value) => typeof value === "string");
      } catch {
        return [];
      }
    })
    .join("");
}

export async function prepare({ page }) {
  await configureLiveModel(page);
}

export async function run({ page, goto, runtimeCheckpoint, expect }) {
  await putAgent(page, AGENT_ID, {
      id: AGENT_ID, name: "Frontend protocol agent", model: { id: LIVE_MODEL_ID },
      system: "Reply with PROTOCOL READY and a short phrase naming the user's request.",
      tools: [], mcp_servers: [], skills: [], plugins: [], plugin_config: {},
      context_policy: { kind: "keep_all" }, max_steps: 4,
  });
  await publishAgent(page, AGENT_ID);
  const session = await createManagedSession(page, {
    agent: AGENT_ID,
    title: "Frontend protocol shared thread",
  }, MANAGED_HEADERS);
  const access = await requireJson(await page.request.post(`${BACKEND}/v1/application-access-tokens`, {
    data: {
      protocols: ["ai-sdk", "ag-ui"],
      operations: ["thread.run", "thread.messages.read"],
      thread_bindings: [{ external_thread_id: THREAD_ID, managed_session_id: session.id }],
      expires_in_seconds: 300,
    },
  }), "Application access token issue");
  const applicationHeaders = { authorization: `Bearer ${access.access_token}` };

  await goto("/w/default/protocols");
  await expect(page.getByText("Vercel AI SDK", { exact: true })).toBeVisible();
  const ai = await page.request.post(`${BACKEND}/v1/ai-sdk/threads/${THREAD_ID}/runs`, {
    headers: applicationHeaders,
    data: {
      threadId: THREAD_ID,
      messages: [{ id: "u-ai", role: "user", parts: [{ type: "text", text: "from AI SDK" }] }],
    },
  });
  await requireOk(ai, "AI SDK Run");
  expect(streamedText(await ai.text())).toContain("PROTOCOL READY");

  await expect(page.getByText("AG-UI", { exact: true })).toBeVisible();
  const ag = await page.request.post(`${BACKEND}/v1/ag-ui/agents/${AGENT_ID}`, {
    headers: applicationHeaders,
    data: {
      threadId: THREAD_ID,
      runId: "ag-run-1",
      messages: [{ id: "u-ag", role: "user", content: "from AG-UI" }],
    },
  });
  await requireOk(ag, "AG-UI Run");
  const agStream = await ag.text();
  expect(agStream).toContain("RUN_STARTED");
  expect(streamedText(agStream)).toContain("PROTOCOL READY");
  expect(agStream).toContain("RUN_FINISHED");

  await runtimeCheckpoint("AI SDK and AG-UI expose one shared committed thread", async () => {
    const aiHistory = await page.request.get(`${BACKEND}/v1/ai-sdk/threads/${THREAD_ID}/messages`, { headers: applicationHeaders });
    const agHistory = await page.request.get(`${BACKEND}/v1/ag-ui/threads/${THREAD_ID}/messages`, { headers: applicationHeaders });
    await requireOk(aiHistory, "AI SDK history");
    await requireOk(agHistory, "AG-UI history");
    const aiBody = await aiHistory.json();
    const agBody = await agHistory.json();
    expect(JSON.stringify(aiBody)).toContain("from AI SDK");
    expect(JSON.stringify(aiBody)).toContain("from AG-UI");
    expect(JSON.stringify(agBody)).toContain("from AI SDK");
    expect(JSON.stringify(agBody)).toContain("from AG-UI");
  });
}
