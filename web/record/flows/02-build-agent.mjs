// Build one unsaved Agent draft, bind a Resource, start an isolated Preview, and
// only then Publish the exact Agent + Resource snapshot. This chapter proves the
// low-friction authoring contract without requiring a model inference.
import { configureSyntheticModel } from "../support/models.mjs";
import { MEMORY_HEADERS } from "../support/betas.mjs";

const RUN = Date.now();
const AGENT_ID = `release-notes-${RUN}`;
const MODEL_ID = "draft-preview-model";
const SYSTEM =
  "You are a release-notes writer. Produce concise notes grouped into Features, " +
  "Fixes, and Breaking changes. Use the mounted release memory for team conventions.";
const RESOURCE_INSTRUCTIONS =
  "Read this store for release-writing conventions and preserve them across sessions.";

export const story = {
  promise: "Build and test a complete Agent with a bound Resource before committing anything to the shared control plane.",
  effect: "An isolated Preview runs from unsaved fields and Resource bindings, then Publish freezes both as one exact snapshot.",
  aha: "Resource bound, Preview ready, nothing saved—then one Publish freezes the exact Agent and Resource versions.",
  loyalty: "Safe experimentation before commitment makes teams comfortable refining more specialists in the same workspace.",
  satisfaction: "Removing mandatory saves and showing the publication contents makes every transition predictable.",
  advocacy: "The visible unsaved-to-preview-to-fingerprint sequence is a compact proof viewers can repeat themselves.",
};

export async function run({ page, goto, say, clearCaption, intro, checkpoint, aha, expect, click, type, wait, beat }) {
  await configureSyntheticModel(page, MODEL_ID);
  const storeResponse = await page.request.post("http://127.0.0.1:38080/v1/memory_stores", {
    headers: MEMORY_HEADERS,
    data: { name: `Release conventions ${RUN}` },
  });
  expect(storeResponse.ok()).toBeTruthy();
  const store = await storeResponse.json();

  await goto("/w/default/agents/new");
  await intro(
    "Experiment with a complete Agent and its Resources before creating shared control-plane state.",
    "Awaken compiles unsaved fields and bindings into an isolated Preview, then publishes one exact combined snapshot.",
  );

  await type(page.getByPlaceholder("coding-agent"), AGENT_ID);
  await page.getByLabel(/Model \(references workspace catalog\)|模型/).selectOption({ label: MODEL_ID });
  await click(page.getByRole("tab", { name: /Build|构建/, exact: true }));
  await click(page.getByRole("tab", { name: /Instructions|提示词/, exact: true }));
  await type(page.getByLabel(/System instructions|系统指令/), SYSTEM, { delay: 10 });

  await click(page.getByRole("tab", { name: /Memory & resources|Memory 与资源/, exact: true }));
  await say("Resources belong to the same browser draft—there is no separate Resource save step.", 3400);
  await click(page.getByRole("button", { name: /bind a store|绑定记忆库/ }));
  await page.getByLabel(/Store|记忆库/).selectOption(store.id);
  await type(page.getByLabel(/Mount path|挂载路径/), "/mnt/memory/releases");
  await type(page.getByLabel(/Instructions for this resource|该资源的注入提示词/), RESOURCE_INSTRUCTIONS, { delay: 9 });

  await checkpoint("the Resource is bound while Agent and bindings are still unsaved", async () => {
    await expect(page.locator(".pill").filter({ hasText: /unsaved|未保存/ })).toBeVisible();
    await expect(page.getByText("Resource changes are part of this Agent draft and are used by Try immediately.")).toBeVisible();
    const config = await page.request.get(`http://127.0.0.1:38080/v1/config/agents/${AGENT_ID}`);
    const resources = await page.request.get(`http://127.0.0.1:38080/v1/config/agents/${AGENT_ID}/resources`);
    expect(config.status()).toBe(404);
    expect(resources.status()).toBe(404);
  });

  await say("Try uses the current fields and Resource binding immediately; Save draft remains optional.", 3400);
  await click(page.getByRole("button", { name: /Try draft|试运行草稿/ }));
  await beat(
    "The drawer states the contract before execution: this temporary snapshot is never saved or published.",
    page.getByText(/isolated, temporary snapshot|隔离的临时快照/),
    3400,
  );
  await click(page.getByRole("button", { name: /Start preview|开始预览/ }));
  await checkpoint("an isolated Preview starts without saving the Agent", async () => {
    await expect(page.getByText("AI SDK", { exact: true })).toBeVisible();
    await expect(page.getByText(/Ask the current Agent draft anything|向当前 Agent 草稿提问/)).toBeVisible();
    const config = await page.request.get(`http://127.0.0.1:38080/v1/config/agents/${AGENT_ID}`);
    const resources = await page.request.get(`http://127.0.0.1:38080/v1/config/agents/${AGENT_ID}/resources`);
    expect(config.status()).toBe(404);
    expect(resources.status()).toBe(404);
  });
  await wait(900);
  await click(page.getByRole("button", { name: /Close|关闭/ }));

  await say("Publish now saves and validates once, then shows the combined snapshot before confirmation.", 3600);
  await click(page.getByRole("button", { name: /Publish|发布/, exact: true }));
  const publishModal = page.locator(".modal");
  let savedConfig;
  let savedResources;
  await checkpoint("the confirmation shows the saved Agent and Resource snapshot together", async () => {
    await expect(publishModal.getByText(/Draft compiled successfully|草稿已通过编译/)).toBeVisible();
    await expect(publishModal.getByText(/Publication snapshot|发布快照/)).toBeVisible();
    await expect(publishModal.getByText("/mnt/memory/releases", { exact: true })).toBeVisible();
    savedConfig = await (await page.request.get(`http://127.0.0.1:38080/v1/config/agents/${AGENT_ID}`)).json();
    savedResources = await (await page.request.get(`http://127.0.0.1:38080/v1/config/agents/${AGENT_ID}/resources`)).json();
    expect(savedConfig.model).toBe(MODEL_ID);
    expect(savedResources.revision).toBe(1);
    expect(savedResources.inputs[0]).toMatchObject({
      target: { kind: "memory_store", id: store.id },
      mount_path: "/mnt/memory/releases",
      access: "read_write",
      instructions: RESOURCE_INSTRUCTIONS,
    });
  });

  const publishResponse = page.waitForResponse((response) =>
    response.url().endsWith(`/v1/config/agents/${AGENT_ID}/publish`) &&
    response.request().method() === "POST",
  );
  await click(publishModal.getByRole("button", { name: /Publish|发布/, exact: true }));
  const publishedHttp = await publishResponse;
  expect(publishedHttp.ok()).toBeTruthy();
  const published = await publishedHttp.json();

  await checkpoint("the publication freezes the exact Agent and Resource revisions", async () => {
    const publicationResponse = await page.request.get(
      `http://127.0.0.1:38080/v1/config/publications/${published.fingerprint}`,
    );
    expect(publicationResponse.ok()).toBeTruthy();
    const publication = await publicationResponse.json();
    expect(publication.source_revision).toBe(savedConfig.generation);
    expect(publication.agent_inputs.revision).toBe(savedResources.revision);
    expect(publication.agent_inputs.inputs).toEqual(savedResources.inputs);
    await expect(page.getByText(new RegExp(published.fingerprint.slice(0, 12)))).toBeVisible();
  });
  await clearCaption();
  await aha(story.aha);
  await wait(900);
  await clearCaption();
}
