// Memory effect, not just binding UI: one real-model session writes a random fact,
// the store is harvested, and a fresh session recalls it with no shared chat history.
import { LIVE_MODEL_ID, configureLiveModel } from "../support/models.mjs";

const AGENT = "release-notes-writer";

export const story = {
  promise: "Teach an Agent one fact and prove a completely fresh session can recall it without shared chat history.",
  effect: "Session one writes a random code to an explicit Memory mount and session two reads the same code back.",
  aha: "The second session knows what the first one learned—because Memory is a mounted resource, not hidden chat history.",
  loyalty: "Durable, inspectable continuity makes the Agent more useful over time and rewards continued use.",
  satisfaction: "A random-code proof removes ambiguity about whether Memory binding has a real runtime effect.",
  advocacy: "Fresh-session recall is an instantly understandable demonstration viewers can repeat for colleagues.",
};

export async function run({ page, goto, say, clearCaption, intro, runtimeCheckpoint, aha, expect, click, type, wait, beat }) {
  await configureLiveModel(page);
  const secret = `AHA-${Date.now()}`;
  const store = await (await page.request.post("http://127.0.0.1:38080/v1/memory_stores", {
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
  await click(page.getByRole("tab", { name: /Resources|资源/, exact: true }));
  await say("Bind an explicit read-write Memory store; the mount path and access stay visible in Agent config.", 3800);
  await click(page.getByRole("button", { name: /bind a store|绑定记忆库/ }));
  await page.locator("select").nth(1).selectOption({ label: store.name });
  await type(page.getByPlaceholder("/mnt/…"), "/mnt/memory/project");
  await click(page.getByRole("button", { name: /Save resources|保存资源/ }));
  await wait(600);
  const publish = await page.request.post(`http://127.0.0.1:38080/v1/config/agents/${AGENT}/publish`);
  expect(publish.ok()).toBeTruthy();

  const first = await (await page.request.post("http://127.0.0.1:38080/v1/sessions", {
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
    const persisted = await (await page.request.get(`http://127.0.0.1:38080/v1/memory_stores/${store.id}`)).json();
    expect(persisted.content ?? "").toContain(secret);
  });

  const second = await (await page.request.post("http://127.0.0.1:38080/v1/sessions", {
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
