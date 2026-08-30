// Connect a trusted backend with the official Anthropic SDK, send one useful
// developer handoff, and inspect the exact same durable Session in Console.

import { createRequire } from "node:module";
import { resolve } from "node:path";
import { setTimeout as delay } from "node:timers/promises";
import { LIVE_MODEL_ID, configureLiveModel } from "../support/models.mjs";
import { MANAGED_HEADERS } from "../support/betas.mjs";
import { BACKEND, publishAgent, putAgent, requireOk } from "../support/control-plane.mjs";

const requireFromE2e = createRequire(resolve(import.meta.dirname, "../../../e2e/package.json"));
const Anthropic = requireFromE2e("@anthropic-ai/sdk").default;

const AGENT_ID = "sdk-integration-handoff";
const SESSION_TITLE = "Official SDK integration handoff";
const TASK = `Prepare a developer handoff from these accepted facts:
- The trusted backend uses @anthropic-ai/sdk with its baseURL pointed at Awaken.
- The client created this Session through beta.sessions.create.
- The client sent this user.message through beta.sessions.events.send.
- The request marker is SDK-HANDOFF-27.

Use exactly these headings: Result, Integration boundary, Next step. Include SDK-HANDOFF-27. Do not claim facts beyond this message.`;

export const story = {
  job: "Connect an existing backend",
  stakes: "An integration is incomplete if SDK work is split across histories that developers cannot hand off or inspect together.",
  handoff: "The official Anthropic SDK creates and runs one Session; Console opens that same identity, request, and committed result.",
  promise: "Point the official Anthropic SDK at Awaken and keep application work visible in the Console.",
  effect: "One backend request becomes a durable Session with a developer handoff and inspectable history.",
  aha: "The official SDK starts the work, and the same Session is ready to inspect or hand off in the Console.",
  loyalty: "Existing backend code can keep its SDK while Awaken adds one durable operational record.",
  satisfaction: "The integration returns a committed result and the HTTP receipt that accepted it.",
  advocacy: "A developer can show the exact request and result without reconstructing them from logs.",
};

let prepared;

async function listEvents(client, sessionId) {
  const events = [];
  for await (const event of client.beta.sessions.events.list(sessionId)) events.push(event);
  return events;
}

export async function prepare({ page }) {
  await configureLiveModel(page);
  await putAgent(page, AGENT_ID, {
    id: AGENT_ID,
    name: "SDK integration handoff",
    description: "Converts a trusted backend request into a concise, inspectable developer handoff.",
    model: { id: LIVE_MODEL_ID },
    system: "Use only the supplied facts. Follow the requested headings exactly. Keep the handoff under 100 words. Never call tools or invent implementation state.",
    tools: [],
    tool_overrides: [],
    mcp_servers: [],
    skills: [],
    max_steps: 4,
    plugins: ["compact"],
    plugin_config: { compact: {} },
    context_policy: { kind: "keep_last", keep_last: 12 },
  });
  await publishAgent(page, AGENT_ID);

  // In no-login all-in-one, the local placeholder is intentionally accepted.
  // Self-managed recordings supply a real Workspace service key via the named
  // environment variable; neither path exposes credential material on screen.
  const client = new Anthropic({
    apiKey: process.env.AWAKEN_RECORD_SERVICE_API_KEY ?? "local-no-login", // awaken-allow: secret
    baseURL: BACKEND,
    maxRetries: 0,
  });
  const session = await client.beta.sessions.create({
    agent: AGENT_ID,
    environment_id: "env_local",
    title: SESSION_TITLE,
    metadata: { source: "official-anthropic-sdk", request_marker: "SDK-HANDOFF-27" },
  });
  const receipt = await client.beta.sessions.events.send(session.id, {
    events: [{ type: "user.message", content: [{ type: "text", text: TASK }] }],
  });
  const acceptedEventId = receipt.data[0]?.id;
  if (typeof acceptedEventId !== "string") {
    throw new Error(`official SDK did not return an accepted Event id: ${JSON.stringify(receipt)}`);
  }

  const deadline = Date.now() + 420_000;
  while (Date.now() < deadline) {
    const events = await listEvents(client, session.id);
    const failure = events.find((event) => event.type === "session.error");
    if (failure) throw new Error(`official SDK Session failed: ${JSON.stringify(failure)}`);
    const answer = events.find((event) => event.type === "agent.message"
      && (event.content ?? []).some((content) => {
        const text = content.text ?? "";
        return /Result[\s\S]*Integration boundary[\s\S]*Next step/i.test(text)
          && /SDK-HANDOFF-27/i.test(text);
      }));
    const idle = events.some((event) => event.type === "session.status_idle");
    if (answer && idle) {
      prepared = { session, acceptedEventId, events };
      return;
    }
    await delay(1_000);
  }
  throw new Error("official SDK integration handoff did not complete within 420000ms");
}

export async function run({ page, goto, intro, beat, say, clearCaption, checkpoint, aha, expect, click, wait }) {
  if (!prepared) await prepare({ page });
  const { session, acceptedEventId } = prepared;

  await goto("/w/default/protocols");
  await intro(
    "An existing backend needs durable Agent work without creating a second, disconnected history.",
    "Point the official Anthropic SDK at Awaken, then open the same Session in the Console.",
  );
  await say("Fixed test data. The official SDK call and Agent run are live.", 3000);

  const managedCard = page.locator(".card").filter({ has: page.getByRole("heading", { name: "Managed Agents", exact: true }) });
  await beat(
    "Managed Agents is for trusted backends that need durable work.",
    managedCard.getByText(/A trusted backend must start durable Agent work/),
    2600,
  );
  const managedHelp = managedCard.locator("details.protocol-help");
  if (!(await managedHelp.evaluate((element) => element.open))) {
    await click(managedHelp.getByText("How to connect Managed Agents", { exact: true }));
  }
  const sdkExample = managedHelp.locator("pre.code-block");
  await checkpoint("the Console documents the official SDK and its service-key boundary", async () => {
    await expect(sdkExample).toContainText('@anthropic-ai/sdk');
    await expect(sdkExample).toContainText("beta.sessions.create");
    await expect(sdkExample).toContainText("beta.sessions.events.send");
    const accessLink = managedCard.getByRole("link", { name: /Create or manage service API keys/ });
    if (await accessLink.isVisible().catch(() => false)) {
      await expect(accessLink).toBeVisible();
    } else {
      await expect(managedCard).toContainText("No key required in local no-login mode");
    }
    await expect(managedCard.getByRole("link", { name: /Read Managed Agents documentation/ })).toHaveAttribute(
      "href",
      "https://awakenworks.com/docs/agents/protocols/managed-agents/",
    );
  });
  await beat("The trusted backend creates a Session and sends its task. The service key never enters browser code.", sdkExample, 3400);

  await goto(`/w/default/sessions/${session.id}`);
  const technicalId = page.locator("details.technical-id").filter({ hasText: session.id });
  await click(technicalId.getByText(/Technical ID|技术 ID/, { exact: true }));
  await checkpoint("the API-created Session keeps the exact official-SDK identity and request", async () => {
    await expect(page.getByText(SESSION_TITLE, { exact: true })).toBeVisible({ timeout: 15_000 });
    await expect(technicalId.getByText(session.id, { exact: true })).toBeVisible({ timeout: 15_000 });
    const read = await page.request.get(`${BACKEND}/v1/sessions/${session.id}`, { headers: MANAGED_HEADERS });
    await requireOk(read, "official SDK Session Console readback");
    const body = await read.json();
    expect(body.agent.id).toBe(AGENT_ID);
    expect(body.metadata).toMatchObject({ source: "official-anthropic-sdk", request_marker: "SDK-HANDOFF-27" });
    const events = await page.request.get(`${BACKEND}/v1/sessions/${session.id}/events`, { headers: MANAGED_HEADERS });
    await requireOk(events, "official SDK Event Console readback");
    expect((await events.json()).data.some((event) => event.id === acceptedEventId && event.type === "user.message")).toBeTruthy();
  });
  await beat("The Console opens the exact Session created by the SDK.", technicalId, 2800);

  const answer = page.locator('[data-role="assistant"]').last();
  await checkpoint("the backend request ends in one committed developer handoff", async () => {
    await expect(answer).toContainText(/Result[\s\S]*Integration boundary[\s\S]*Next step/i, { timeout: 15_000 });
    await expect(answer).toContainText("SDK-HANDOFF-27", { timeout: 15_000 });
  });
  await beat("The committed handoff keeps the request marker, integration boundary, and next step together.", answer, 3400);
  await say("Share this Session instead of reconstructing work from application logs.", 3000);

  await clearCaption();
  await aha(story.aha, 5000);
  await wait(800);
  await clearCaption();
}
