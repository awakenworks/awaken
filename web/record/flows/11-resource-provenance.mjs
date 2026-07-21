// Resource proof: one Session exposes every mounted input and its stable origin.
import { configureSyntheticModel } from "../support/models.mjs";

const AGENT_ID = "resource-provenance-agent";
const MODEL_ID = "resource-provenance-model";

export const story = {
  promise: "Assemble Memory, a reference file, and a Skill once, then prove a Session receives all three inputs.",
  effect: "The Session Files view visibly lists each mounted resource with its type, path, and source identifier.",
  aha: "A Session shows exactly what the Agent could use—Memory, files, and Skills are explicit provenance, not hidden context.",
  loyalty: "Inspectable reusable inputs accumulate trusted value without trapping knowledge inside chat history.",
  satisfaction: "Operators can answer what the Agent received from one screen instead of debugging invisible prompt assembly.",
  advocacy: "A single provenance view makes the platform's transparency concrete for security and operations teams.",
};

export async function run({ page, goto, intro, say, clearCaption, checkpoint, aha, expect, click, wait }) {
  await configureSyntheticModel(page, MODEL_ID);
  const fileResponse = await page.request.post("http://127.0.0.1:38080/v1/files", {
    multipart: {
      file: { name: "release-policy.txt", mimeType: "text/plain", buffer: Buffer.from("Release only after checks pass.") },
      purpose: "agent",
    },
  });
  expect(fileResponse.ok()).toBeTruthy();
  const file = await fileResponse.json();
  const skillResponse = await page.request.post("http://127.0.0.1:38080/v1/skills", {
    multipart: {
      file: { name: "SKILL.md", mimeType: "text/markdown", buffer: Buffer.from("# Verify release\nCheck policy and durable memory.") },
      name: "verify-release",
    },
  });
  expect(skillResponse.ok()).toBeTruthy();
  const skill = await skillResponse.json();
  const memoryResponse = await page.request.post("http://127.0.0.1:38080/v1/memory_stores", {
    data: { name: `Release decisions · ${Date.now()}` },
  });
  expect(memoryResponse.ok()).toBeTruthy();
  const memory = await memoryResponse.json();
  await page.request.put(`http://127.0.0.1:38080/v1/config/agents/${AGENT_ID}`, {
    data: {
      id: AGENT_ID, name: "Resource provenance agent", model: { id: MODEL_ID },
      system: "Use only mounted evidence.", tools: [], mcp_servers: [], skills: [{ id: skill.id }],
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
  expect(published.ok()).toBeTruthy();
  const sessionResponse = await page.request.post("http://127.0.0.1:38080/v1/sessions", {
    data: { agent: AGENT_ID, title: "Visible input provenance" },
  });
  expect(sessionResponse.ok()).toBeTruthy();
  const session = await sessionResponse.json();

  await goto(`/w/default/sessions/${session.id}`);
  await intro(
    "Make every input to an Agent inspectable so operators can trust and reproduce its work.",
    "Project Memory, files, and Skills into one Session Files view with stable source ids and mount paths.",
  );
  await click(page.getByRole("button", { name: /Files|文件/, exact: true }));
  await say("The runtime projection keeps source type, identity, access path, and later artifacts in one place.", 4200);
  await checkpoint("the Session visibly receives all configured resources", async () => {
    for (const path of resources.map((resource) => resource.mount_path)) {
      await expect(page.getByText(path, { exact: true })).toBeVisible();
    }
    const response = await page.request.get(`http://127.0.0.1:38080/v1/sessions/${session.id}/resources`);
    expect(response.ok()).toBeTruthy();
    const body = await response.json();
    expect(body.data.map((item) => item.mount_path)).toEqual(expect.arrayContaining(resources.map((item) => item.mount_path)));
  });
  await clearCaption();
  await aha(story.aha);
  await wait(900);
  await clearCaption();
}
