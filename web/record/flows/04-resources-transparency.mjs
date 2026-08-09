// Memory effect, not just binding UI: one real-model session writes a random fact,
// the store is harvested, and a fresh session recalls it with no shared chat history.
import { LIVE_MODEL_ID, configureLiveModel } from "../support/models.mjs";
import { MANAGED_HEADERS, MEMORY_HEADERS } from "../support/betas.mjs";

const RUN = Date.now();
const AGENT = `release-memory-keeper-${RUN}`;

export const story = {
  promise: "Teach an Agent one fact and prove a completely fresh session can recall it without shared chat history.",
  effect: "Session one writes a random code to an explicit Memory mount and session two reads the same code back.",
  aha: "The second session knows what the first one learned—because Memory is a mounted resource, not hidden chat history.",
  loyalty: "Durable, inspectable continuity makes the Agent more useful over time and rewards continued use.",
  satisfaction: "A random-code proof removes ambiguity about whether Memory binding has a real runtime effect.",
  advocacy: "Fresh-session recall is an instantly understandable demonstration viewers can repeat for colleagues.",
};

export async function run({ page, goto, say, clearCaption, intro, checkpoint, runtimeCheckpoint, aha, expect, click, type, wait, beat }) {
  await configureLiveModel(page);
  const secret = `AHA-${Date.now()}`;
  const store = await (await page.request.post("http://127.0.0.1:38080/v1/memory_stores", {
    headers: MEMORY_HEADERS,
    data: { name: `release-memory-${Date.now()}` },
  })).json();
  await page.request.put(`http://127.0.0.1:38080/v1/config/agents/${AGENT}`, {
    data: {
      id: AGENT,
      name: "Release memory keeper",
      model: { id: LIVE_MODEL_ID },
      system: "Use the persistent memory file. WRITE exact facts the user asks you to remember; READ it to recall. Always use file tools.",
      tools: ["bash", "read", "write", "glob", "grep"],
      plugins: [],
      plugin_config: { permission: { default_behavior: "allow", mode: "bypassPermissions", rules: [] } },
      context_policy: { kind: "keep_all" },
      max_steps: 8,
    },
  });

  await goto(`/w/default/agents/${AGENT}`);
  await intro(
    "Give an Agent durable memory and prove it survives beyond the conversation that created it.",
    "Awaken mounts a first-class Memory store, writes it back after execution, and recalls it in every fresh Agent session.",
  );
  await click(page.getByRole("tab", { name: /Build|构建/, exact: true }));
  await click(page.getByRole("tab", { name: /Memory & resources|Memory 与资源/, exact: true }));
  await say("Bind an explicit read-write Memory store; the mount path and access stay visible in Agent config.", 3800);
  await click(page.getByRole("button", { name: /bind a store|绑定记忆库/ }));
  await page.getByLabel(/Store|记忆库/).selectOption(store.id);
  await type(page.getByLabel(/Mount path|挂载路径/), "/mnt/memory/project");
  await type(
    page.getByLabel(/Instructions for this resource|该资源的注入提示词/),
    "Write requested facts here and read them back in every fresh session.",
    { delay: 9 },
  );
  await say("There is no separate Resource save. Publish saves Agent config and bindings, validates them, and shows one snapshot.", 4000);
  await click(page.getByRole("button", { name: /Publish|发布/, exact: true }));
  const publishModal = page.locator(".modal");
  let savedConfig;
  let savedResources;
  await checkpoint("the publication confirmation includes the exact Memory binding", async () => {
    await expect(publishModal.getByText(/Publication snapshot|发布快照/)).toBeVisible();
    await expect(publishModal.getByText("/mnt/memory/project", { exact: true })).toBeVisible();
    savedConfig = await (await page.request.get(`http://127.0.0.1:38080/v1/config/agents/${AGENT}`)).json();
    savedResources = await (await page.request.get(`http://127.0.0.1:38080/v1/config/agents/${AGENT}/resources`)).json();
    expect(savedResources.revision).toBe(1);
    expect(savedResources.inputs[0].target.id).toBe(store.id);
  });
  const publishResponse = page.waitForResponse((response) =>
    response.url().endsWith(`/v1/config/agents/${AGENT}/publish`) &&
    response.request().method() === "POST",
  );
  await click(publishModal.getByRole("button", { name: /Publish|发布/, exact: true }));
  const publishHttp = await publishResponse;
  expect(publishHttp.ok()).toBeTruthy();
  const publish = await publishHttp.json();
  await checkpoint("the published fingerprint freezes Agent config and Resource revision 1", async () => {
    const response = await page.request.get(`http://127.0.0.1:38080/v1/config/publications/${publish.fingerprint}`);
    expect(response.ok()).toBeTruthy();
    const publication = await response.json();
    expect(publication.source_revision).toBe(savedConfig.generation);
    expect(publication.agent_inputs.revision).toBe(savedResources.revision);
    expect(publication.agent_inputs.inputs).toEqual(savedResources.inputs);
  });

  const first = await (await page.request.post("http://127.0.0.1:38080/v1/sessions", {
    headers: MANAGED_HEADERS,
    data: { agent: AGENT, title: "Write durable memory" },
  })).json();
  await goto(`/w/default/sessions/${first.id}`);
  await beat("Session one receives a concise goal; the Agent handles the file location and write procedure.", page.locator(".transcript-composer"), 3400);
  const firstComposer = page.getByLabel(/Message to agent|给 Agent 的消息/);
  await type(firstComposer, `Remember this exact release code: ${secret}`, { delay: 16 });
  await firstComposer.press("Enter");
  await expect(page.locator(".agent-working")).toBeVisible();
  await say("Immediate working feedback keeps the conversation alive while the real model and tools execute.", 3400);

  await runtimeCheckpoint("session one writes the random code into the bound durable store", async () => {
    await expect(page.locator(".agent-working")).not.toBeVisible({ timeout: 60000 });
    await expect(page.getByText("⬡ agent").last()).toBeVisible();
    await page.request.get(`http://127.0.0.1:38080/v1/files?scope_id=${first.id}`);
    const persisted = await (await page.request.get(
      `http://127.0.0.1:38080/v1/memory_stores/${store.id}`,
      { headers: MEMORY_HEADERS },
    )).json();
    expect(persisted.content ?? "").toContain(secret);
  });

  const second = await (await page.request.post("http://127.0.0.1:38080/v1/sessions", {
    headers: MANAGED_HEADERS,
    data: { agent: AGENT, title: "Recall durable memory" },
  })).json();
  await goto(`/w/default/sessions/${second.id}`);
  await say("Session two is fresh—no shared transcript. Ask for the fact using only the bound Memory.", 3800);
  const secondComposer = page.getByLabel(/Message to agent|给 Agent 的消息/);
  await type(secondComposer, "Read your persistent memory and answer with only the exact release code.", { delay: 12 });
  await secondComposer.press("Enter");

  await runtimeCheckpoint("a fresh Agent session recalls the exact code from Memory", async () => {
    await expect(page.getByText(secret, { exact: false })).toBeVisible({ timeout: 60000 });
    await expect(page.locator(".agent-working")).not.toBeVisible({ timeout: 10000 });
  });
  await click(page.getByRole("button", { name: "Trace", exact: true }));
  await say("Trace keeps the evidence inspectable after the payoff: model, tools, and lifecycle remain observable.", 3600);
  await clearCaption();
  await aha(story.aha);
  await wait(800);
  await clearCaption();
}
