// Managed Agents proof: create through the public wire and observe in the console.
import { configureSyntheticModel } from "../support/models.mjs";
import { MANAGED_HEADERS } from "../support/betas.mjs";
import { BACKEND, createManagedSession, publishAgent, putAgent, requireOk } from "../support/control-plane.mjs";

const AGENT_ID = "managed-api-agent";
const MODEL_ID = "managed-api-recording-model";

export async function run({ page, goto, checkpoint, expect }) {
  await configureSyntheticModel(page, MODEL_ID);
  await putAgent(page, AGENT_ID, {
      id: AGENT_ID, name: "Managed API agent", model: { id: MODEL_ID },
      system: "Serve API clients.", tools: [], mcp_servers: [], skills: [],
      plugins: [], plugin_config: {}, context_policy: { kind: "keep_all" }, max_steps: 4,
  });
  await publishAgent(page, AGENT_ID);

  await goto("/w/default/sessions");
  const session = await createManagedSession(page, {
    agent: AGENT_ID,
    title: "Created through Managed Agents API",
    metadata: { source: "managed-api-proof" },
  }, MANAGED_HEADERS);
  await goto(`/w/default/sessions/${session.id}`);
  const technicalId = page.locator("details.technical-id").filter({ hasText: session.id });
  await technicalId.getByText(/Technical ID|技术 ID/, { exact: true }).click();
  await checkpoint("the console observes the exact API-created Session", async () => {
    await expect(technicalId.getByText(session.id, { exact: true })).toBeVisible();
    await expect(page.getByText("Created through Managed Agents API", { exact: true })).toBeVisible();
    await expect(page.getByText("Managed API agent", { exact: true })).toBeVisible();
    const read = await page.request.get(`${BACKEND}/v1/sessions/${session.id}`, { headers: MANAGED_HEADERS });
    await requireOk(read, "Managed API Session readback");
    const body = await read.json();
    expect(body.agent.id).toBe(AGENT_ID);
    expect(body.metadata.source).toBe("managed-api-proof");
  });
}
