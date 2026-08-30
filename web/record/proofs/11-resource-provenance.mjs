// Resource-provenance proof: one Session exposes every mounted input and its stable origin.
import { configureSyntheticModel } from "../support/models.mjs";
import { FILES_HEADERS, MANAGED_HEADERS, MEMORY_HEADERS } from "../support/betas.mjs";
import { BACKEND, createManagedSession, publishAgent, putAgent, putAgentResources, requireJson, requireOk } from "../support/control-plane.mjs";

const RUN = Date.now();
const AGENT_ID = `resource-provenance-${RUN}`;
const MODEL_ID = "resource-provenance-model";

export async function run({ page, goto, intro, say, clearCaption, checkpoint, aha, expect, click, type, wait }) {
  await configureSyntheticModel(page, MODEL_ID);
  const file = await requireJson(await page.request.post(`${BACKEND}/v1/files`, {
    headers: FILES_HEADERS,
    multipart: {
      file: { name: "release-policy.txt", mimeType: "text/plain", buffer: Buffer.from("Release only after checks pass.") },
    },
  }), "reference File upload");
  const memory = await requireJson(await page.request.post(`${BACKEND}/v1/memory_stores`, {
    headers: MEMORY_HEADERS,
    data: { name: `Release decisions · ${RUN}` },
  }), "Memory Store create");
  await putAgent(page, AGENT_ID, {
      id: AGENT_ID, name: "Resource provenance agent", model: { id: MODEL_ID },
      system: "Use only mounted evidence.", tools: [], mcp_servers: [], skills: [],
      plugins: [], plugin_config: {}, context_policy: { kind: "keep_all" }, max_steps: 4,
  });
  const inputs = [
    { binding_id: "memory", target: { kind: "memory_store", id: memory.id }, mount_path: "/mnt/memory/releases", access: "read_write" },
    // The production-default Namespace sandbox can enforce this read-only
    // binding. The recorder must never downgrade to the unsandboxed local tier,
    // whose Worker manifest would correctly make this run remain pending.
    { binding_id: "file", target: { kind: "file", id: file.id }, mount_path: "/mnt/files/release-policy.txt", access: "read_only" },
  ];
  await putAgentResources(page, AGENT_ID, inputs);
  await publishAgent(page, AGENT_ID);
  const session = await createManagedSession(page, { agent: AGENT_ID, title: "Visible input provenance" }, MANAGED_HEADERS);

  await goto(`/w/default/sessions/${session.id}`);
  await intro(
    "A release recommendation needs the exact policy file and Memory behind it.",
    "Session Inputs shows both sources, their access mode, and where the Agent received them.",
  );
  const composer = page.getByLabel(/Message to agent|给 Agent 的消息/);
  await expect(composer).toBeEnabled({ timeout: 60_000 });
  await type(composer, "Inspect the mounted release inputs and wait for operator review.", { delay: 12 });
  await composer.press("Enter");
  await expect.poll(async () => {
    const response = await page.request.get(
      `${BACKEND}/v1/sessions/${session.id}/resources`,
      { headers: MANAGED_HEADERS },
    );
    await requireOk(response, "Session Resource realization receipt");
    return (await response.json()).data.map((item) => item.mount_path).sort();
  }, { timeout: 30_000 }).toEqual(inputs.map((item) => item.mount_path).sort());
  await click(page.getByRole("button", { name: /Inputs|输入/, exact: true }));
  await say("Inputs exposes both sources before anyone approves the recommendation.", 3200);
  await checkpoint("the Session visibly receives both configured Resources", async () => {
    for (const path of inputs.map((resource) => resource.mount_path)) {
      await expect(page.getByText(path, { exact: true })).toBeVisible();
    }
    const response = await page.request.get(
      `${BACKEND}/v1/sessions/${session.id}/resources`,
      { headers: MANAGED_HEADERS },
    );
    await requireOk(response, "Session Resource provenance readback");
    const body = await response.json();
    expect(body.data.map((item) => item.mount_path)).toEqual(expect.arrayContaining(inputs.map((item) => item.mount_path)));
  });
  await clearCaption();
  await wait(900);
  await clearCaption();
}
