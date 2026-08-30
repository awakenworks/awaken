// Memory-effect proof: one real-model session writes a durable policy,
// the store is harvested, and a fresh session recalls it with no shared chat history.
import { LIVE_MODEL_ID, configureLiveModel } from "../support/models.mjs";
import { MANAGED_HEADERS, MEMORY_HEADERS } from "../support/betas.mjs";
import { BACKEND, createManagedSession, putAgent, requireJson, requireOk } from "../support/control-plane.mjs";

const RUN = Date.now();
const AGENT = `release-memory-keeper-${RUN}`;

export async function prepare({ page }) {
  await configureLiveModel(page);
}

export async function run({ page, goto, say, clearCaption, intro, checkpoint, runtimeCheckpoint, aha, expect, click, type, wait, beat }) {
  const durablePolicy = "Human approval is required before every external side effect.";
  const store = await requireJson(await page.request.post(`${BACKEND}/v1/memory_stores`, {
    headers: MEMORY_HEADERS,
    data: { name: `release-memory-${Date.now()}` },
  }), "Memory Store create");
  await putAgent(page, AGENT, {
      id: AGENT,
      name: "Release memory keeper",
      model: { id: LIVE_MODEL_ID },
      system: "Persistent project Memory is mounted at .mnt/mnt/memory/project. For a write request, call write once at .mnt/mnt/memory/project/release-policy.txt. For a recall request, use the recalled durable Memory context and answer concisely. Never inspect directories.",
      tools: ["read", "write"],
      plugins: [],
      plugin_config: { permission: { default_behavior: "allow", mode: "bypassPermissions", rules: [] } },
      context_policy: { kind: "keep_all" },
      max_steps: 3,
  });

  await goto(`/w/default/agents/${AGENT}`);
  await intro(
    "Every external change requires human approval, even after the original Session closes.",
    "Record the rule once, close the Session, then ask a fresh Session to recall it.",
  );
  await click(page.getByRole("tab", { name: /Build|构建/, exact: true }));
  await click(page.getByRole("tab", { name: /Memory & resources|Memory 与资源/, exact: true }));
  await say("The release policy gets one durable, reviewable home.", 2200);
  await click(page.getByRole("button", { name: /bind a store|绑定记忆库/ }));
  await page.getByLabel(/Store|记忆库/).selectOption(store.id);
  await type(page.getByLabel(/Mount path|挂载路径/), "/mnt/memory/project");
  await type(
    page.getByLabel(/Instructions for this resource|该资源的注入提示词/),
    "Write requested facts here and read them back in every fresh session.",
    { delay: 9 },
  );
  await say("The rule's home is reviewed with the Agent before either can be published.", 2200);
  await click(page.getByRole("button", { name: /Review & publish|审阅并发布/, exact: true }));
  const publishModal = page.locator(".modal");
  let savedConfig;
  let savedResources;
  await checkpoint("the publication confirmation includes the exact Memory binding", async () => {
    await expect(publishModal.getByText(/Version used by new Sessions|新 Session 使用的版本/)).toBeVisible();
    await expect(publishModal.getByText("/mnt/memory/project", { exact: true })).toBeVisible();
    savedConfig = await requireJson(await page.request.get(`${BACKEND}/v1/config/agents/${AGENT}`), "Memory Agent readback");
    savedResources = await requireJson(await page.request.get(`${BACKEND}/v1/config/agents/${AGENT}/resources`), "Memory Resource readback");
    expect(savedResources.revision).toBe(1);
    expect(savedResources.inputs[0].target.id).toBe(store.id);
  });
  const isPublishRequest = (request) =>
    new URL(request.url()).pathname.endsWith(`/config/agents/${AGENT}/publish`) &&
    request.method() === "POST";
  const publishRequest = page.waitForRequest(isPublishRequest, { timeout: 60_000 });
  const publishResponse = page.waitForResponse(
    (response) => isPublishRequest(response.request()),
    { timeout: 60_000 },
  );
  await click(publishModal.getByRole("button", { name: /Publish|发布/, exact: true }));
  await publishRequest;
  const publishHttp = await publishResponse;
  expect(publishHttp.ok()).toBeTruthy();
  const publish = await publishHttp.json();
  await checkpoint("the published fingerprint freezes Agent config and Resource revision 1", async () => {
    const response = await page.request.get(`${BACKEND}/v1/config/publications/${publish.fingerprint}`);
    await requireOk(response, "Memory publication readback");
    const publication = await response.json();
    expect(publication.source_revision).toBe(savedConfig.generation);
    expect(publication.agent_inputs.revision).toBe(savedResources.revision);
    expect(publication.agent_inputs.inputs).toEqual(savedResources.inputs);
  });

  const first = await createManagedSession(page, { agent: AGENT, title: "Write durable memory" }, MANAGED_HEADERS);
  await goto(`/w/default/sessions/${first.id}`);
  await beat("The first Session records the policy in durable Memory.", page.locator(".transcript-composer"), 2200);
  const firstComposer = page.getByLabel(/Message to agent|给 Agent 的消息/);
  await expect(firstComposer).toBeEnabled({ timeout: 60_000 });
  await type(firstComposer, `Remember this as a durable project policy for every future session, and write it verbatim to the bound Memory: ${durablePolicy}`, { delay: 12 });
  await firstComposer.press("Enter");
  await say("The real model writes the rule into project Memory.", 2000);

  await runtimeCheckpoint("session one writes the durable approval policy into the bound store", async () => {
    await expect(page.locator('article[data-role="assistant"]').last()).toContainText(
      /human approval.+external side effect/i,
      { timeout: 60_000 },
    );
    await expect.poll(async () => {
      const current = await requireJson(
        await page.request.get(`${BACKEND}/v1/sessions/${first.id}`, { headers: MANAGED_HEADERS }),
        "Memory writer Session readback",
      );
      return current.status;
    }, { timeout: 30_000 }).toBe("idle");
  });

  await say("Archiving closes the Session. The policy remains available.", 2400);
  await click(page.getByRole("button", { name: /Archive|归档/, exact: false }));
  await click(page.getByRole("alertdialog").getByRole("button", { name: /^Archive$|^归档$/ }));
  await checkpoint("the terminal lifecycle writes Memory back to its authoritative store", async () => {
    await expect(page.locator(".ui-status-pill").filter({ hasText: /archived|已归档/ })).toBeVisible({ timeout: 30_000 });
    await expect.poll(async () => {
      const memories = await requireJson(await page.request.get(
        `${BACKEND}/v1/memory_stores/${store.id}/memories?view=full`,
        { headers: MEMORY_HEADERS },
      ), "Memory content readback");
      return (memories.data ?? []).map((memory) => memory.content ?? "").join("\n");
    }, { timeout: 60_000 }).toMatch(/human approval.+external side effect/is);
  });

  const second = await createManagedSession(page, { agent: AGENT, title: "Recall durable memory" }, MANAGED_HEADERS);
  await goto(`/w/default/sessions/${second.id}`);
  await say("The next request starts in a clean Session without the earlier conversation.", 2200);
  const secondComposer = page.getByLabel(/Message to agent|给 Agent 的消息/);
  await expect(secondComposer).toBeEnabled({ timeout: 60_000 });
  await type(secondComposer, "State the durable project policy you learned in the previous session, using one sentence only.", { delay: 12 });
  await secondComposer.press("Enter");

  await runtimeCheckpoint("a fresh Agent session recalls the durable approval policy from Memory", async () => {
    const recalledAnswer = page.locator('article[data-role="assistant"]').last();
    await expect(recalledAnswer).toContainText(/approval/i, { timeout: 45_000 });
    const recalledText = await recalledAnswer.innerText();
    // The model may lead with the controlled action or the approval boundary;
    // require all meaning-bearing facts without prescribing prose order.
    expect(recalledText).toMatch(/human/i);
    expect(recalledText).toMatch(/approval/i);
    expect(recalledText).toMatch(/external side effect/i);
    await expect.poll(async () => {
      const current = await requireJson(
        await page.request.get(`${BACKEND}/v1/sessions/${second.id}`, { headers: MANAGED_HEADERS }),
        "Memory reader Session readback",
      );
      return current.status;
    }, { timeout: 30_000 }).toBe("idle");
  });
  await click(page.getByRole("button", { name: "Trace", exact: true }));
  await say("Trace shows exactly where the recalled rule came from.", 2000);
  await clearCaption();
  await wait(400);
  await clearCaption();
}
