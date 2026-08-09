// Resource proof: one Session exposes every mounted input and its stable origin.
import { configureSyntheticModel } from "../support/models.mjs";
import { MANAGED_HEADERS, MEMORY_HEADERS } from "../support/betas.mjs";

const RUN = Date.now();
const AGENT_ID = `resource-provenance-${RUN}`;
const MODEL_ID = "resource-provenance-model";

export const story = {
  promise: "Mount durable Memory and an immutable reference file once, then prove a Session receives both exact inputs.",
  effect: "The Session Files view visibly lists each mounted Resource with its type, path, and stable source identifier.",
  aha: "A Session shows exactly which Memory and file it received—mounted inputs are explicit provenance, not hidden context.",
  loyalty: "Inspectable reusable inputs accumulate trusted value without trapping knowledge inside chat history.",
  satisfaction: "Operators can answer what the Agent received from one screen instead of debugging invisible prompt assembly.",
  advocacy: "A single provenance view makes the platform's transparency concrete for security and operations teams.",
};

async function requireOk(response, label) {
  if (!response.ok()) {
    throw new Error(`${label} failed: HTTP ${response.status()} ${await response.text()}`);
  }
}

export async function run({ page, goto, intro, say, clearCaption, checkpoint, aha, expect, click, wait }) {
  await configureSyntheticModel(page, MODEL_ID);
  const fileResponse = await page.request.post("http://127.0.0.1:38080/v1/files", {
    multipart: {
      file: { name: "release-policy.txt", mimeType: "text/plain", buffer: Buffer.from("Release only after checks pass.") },
      purpose: "agent",
    },
  });
  await requireOk(fileResponse, "reference File upload");
  const file = await fileResponse.json();
  const memoryResponse = await page.request.post("http://127.0.0.1:38080/v1/memory_stores", {
    headers: MEMORY_HEADERS,
    data: { name: `Release decisions · ${RUN}` },
  });
  await requireOk(memoryResponse, "Memory Store create");
  const memory = await memoryResponse.json();
  await page.request.put(`http://127.0.0.1:38080/v1/config/agents/${AGENT_ID}`, {
    data: {
      id: AGENT_ID, name: "Resource provenance agent", model: { id: MODEL_ID },
      system: "Use only mounted evidence.", tools: [], mcp_servers: [], skills: [],
      plugins: [], plugin_config: {}, context_policy: { kind: "keep_all" }, max_steps: 4,
    },
  });
  const inputs = [
    { binding_id: "memory", target: { kind: "memory_store", id: memory.id }, mount_path: "/mnt/memory/releases", access: "read_write" },
    { binding_id: "file", target: { kind: "file", id: file.id }, mount_path: "/mnt/files/release-policy.txt", access: "read_only" },
  ];
  await page.request.put(`http://127.0.0.1:38080/v1/config/agents/${AGENT_ID}/resources`, {
    data: { agent_id: AGENT_ID, inputs, revision: 1 },
  });
  const published = await page.request.post(`http://127.0.0.1:38080/v1/config/agents/${AGENT_ID}/publish`);
  await requireOk(published, "Agent publication");
  const sessionResponse = await page.request.post("http://127.0.0.1:38080/v1/sessions", {
    headers: MANAGED_HEADERS,
    data: { agent: AGENT_ID, title: "Visible input provenance" },
  });
  await requireOk(sessionResponse, "Managed Session create");
  const session = await sessionResponse.json();

  await goto(`/w/default/sessions/${session.id}`);
  await intro(
    "Make every input to an Agent inspectable so operators can trust and reproduce its work.",
    "Project Memory and files into one Session Files view with stable source ids and mount paths.",
  );
  await click(page.getByRole("button", { name: /Files|文件/, exact: true }));
  await say("The runtime projection keeps source type, identity, access path, and later artifacts in one place.", 4200);
  await checkpoint("the Session visibly receives both configured Resources", async () => {
    for (const path of inputs.map((resource) => resource.mount_path)) {
      await expect(page.getByText(path, { exact: true })).toBeVisible();
    }
    const response = await page.request.get(
      `http://127.0.0.1:38080/v1/sessions/${session.id}/resources`,
      { headers: MANAGED_HEADERS },
    );
    expect(response.ok()).toBeTruthy();
    const body = await response.json();
    expect(body.data.map((item) => item.mount_path)).toEqual(expect.arrayContaining(inputs.map((item) => item.mount_path)));
  });
  await clearCaption();
  await aha(story.aha);
  await wait(900);
  await clearCaption();
}
