// Build an API-compatibility Agent from an unsaved draft, run one real contract
// review, and only then publish the exact Agent snapshot.
import { LIVE_MODEL_ID, configureLiveModel } from "../support/models.mjs";
import { BACKEND, requireJson, requireOk } from "../support/control-plane.mjs";

const RUN = Date.now();
const AGENT_ID = `api-compatibility-${String(RUN).slice(-4)}`;
const MODEL_ID = LIVE_MODEL_ID;
const SYSTEM =
  "Review the supplied API contract change using only the supplied facts. Use exactly these headings: Compatibility, Breaking changes, Client impact, Migration actions, Publish decision. " +
  "The user message is the complete evidence set. Do not gather more information, delegate, call, or mention tools. " +
  "Name removed fields and changed enum values exactly. Write no preamble and stay under 80 words.";
const FIRST_TASK = "API change set: GET /v1/tasks/{id} removes result_url; status changes from pending, running, completed to queued, running, succeeded, failed; no compatibility alias or migration note is provided. Assess whether this contract is safe to publish.";

export const story = {
  job: "Agent configuration and publication",
  stakes: "A removed response field and renamed status values can break existing clients if a reusable reviewer overlooks either contract change.",
  handoff: "The result is a reviewed Agent revision and a compatibility brief that names both client breaks and the migration work required before publication.",
  promise: "Build and test an API compatibility reviewer entirely in the Console before publishing it.",
  effect: "The draft identifies both breaking changes, explains their client impact, and becomes reusable only after review.",
  aha: "The reviewer catches both client breaks and names the migration work before you publish it.",
  loyalty: "The reviewer can improve in Preview, while only the examined version becomes reusable.",
  satisfaction: "The first run produces an actionable compatibility decision after the configuration is complete.",
  advocacy: "A contract diff becomes a migration brief that another developer can verify.",
};

export async function prepare({ page }) {
  await configureLiveModel(page);
}

export async function run({ page, goto, say, clearCaption, intro, checkpoint, runtimeCheckpoint, aha, expect, click, type, wait, beat }) {
  await goto("/w/default/agents/new");
  await intro(
    "This API change removes a field and renames statuses. A reusable reviewer must catch both before clients break.",
    "Configure the model, instructions, tools, integrations, context, and release in one Console.",
  );
  await say("Fixed contract data. Preview, response, events, and publication are live.", 3600);

  await type(page.getByPlaceholder("coding-agent"), AGENT_ID);
  await page.getByLabel(/^Model$|^模型$/).selectOption({ label: MODEL_ID });
  await click(page.getByRole("tab", { name: /Build|构建/, exact: true }));
  await type(page.getByPlaceholder("Coding Assistant"), "API compatibility reviewer");
  await click(page.getByRole("tab", { name: /Instructions|提示词/, exact: true }));
  await say("Model, reasoning, speed, and limits belong to this Agent revision.", 3000);
  await page.getByLabel(/Reasoning effort|推理强度/).selectOption("medium");
  await page.getByLabel(/Inference speed|推理速度/).selectOption("standard");
  const systemInstructions = page.getByLabel(/System instructions|系统指令/);
  await click(systemInstructions);
  await systemInstructions.fill(SYSTEM);
  await page.getByLabel(/Max steps|最大步数/).fill("10");
  await click(page.getByRole("button", { name: /Keep last N|保留最近 N 条/ }));
  await page.getByLabel(/Messages kept|保留消息数/).fill("18");
  await beat(
    "Read the diff. Name each break. Return concrete migration actions.",
    systemInstructions,
    2600,
  );

  await click(page.getByRole("tab", { name: /Tools & permissions|工具与权限/ }));
  await beat(
    "Tools, default permissions, failure handling, and deferred loading are visible here.",
    page.getByText(/Advanced tool sources and recovery|高级工具来源与恢复策略/),
    2800,
  );
  await click(page.getByRole("tab", { name: /Skills & MCP|Skills 与 MCP/ }));
  await beat(
    "Skills, MCP, and credential sources are reviewable here.",
    page.getByText(/MCP integrations|MCP 集成/),
    2800,
  );

  await click(page.getByRole("tab", { name: /Advanced|高级/, exact: true }));
  const recursiveAgent = page.getByLabel(/Built-in auxiliary Agent|内置辅助 Agent/);
  if (await recursiveAgent.isChecked()) await click(recursiveAgent);
  await say("This contract review stays with one Agent. Delegation is off.", 2600);
  await click(page.getByRole("tab", { name: /Plugin configuration|Plugin 配置/ }));
  const compactionSwitch = page.getByRole("switch", { name: /Configure|配置/ });
  if (!(await compactionSwitch.isChecked())) await click(compactionSwitch);
  await page.getByLabel(/Trigger window \(tokens\)|触发窗口（token）/).fill("32000");
  await page.getByLabel(/Recent messages kept verbatim|原文保留的最近消息/).fill("8");
  await beat(
    "Long work keeps recent messages exact and summarizes older context.",
    page.getByText(/Context compaction strategy|上下文压缩策略/),
    2800,
  );
  await click(page.getByRole("tab", { name: /Release & diff|发布与差异/ }));
  await beat(
    "The release diff shows exactly what new Sessions will inherit.",
    page.getByText(/Draft versus published release|草稿与已发布版本/),
    2600,
  );
  await click(page.getByRole("tab", { name: /Build|构建/, exact: true }));

  await checkpoint("the compatibility reviewer remains an unsaved draft before Preview", async () => {
    await expect(page.locator(".pill").filter({ hasText: /unsaved|未保存/ })).toBeVisible();
    const config = await page.request.get(`${BACKEND}/v1/config/agents/${AGENT_ID}`);
    expect(config.status()).toBe(404);
  });

  await say("Run the draft before anyone can start a Session from it.", 2400);
  await click(page.getByRole("button", { name: /Try draft|试运行草稿/ }));
  await beat(
    "Preview uses an isolated snapshot. The published Agent remains unchanged.",
    page.getByText(/isolated, temporary snapshot|隔离的临时快照/),
    2600,
  );
  const previewRequest = page.waitForRequest(
    (request) => request.method() === "POST" && /\/config\/agent-previews\/preview-/.test(new URL(request.url()).pathname),
    { timeout: 60_000 },
  );
  await click(page.getByRole("button", { name: /Start preview|开始预览/ }));
  const previewDraft = (await previewRequest).postDataJSON();
  expect(previewDraft.config.multiagent ?? null).toBeNull();
  expect(previewDraft.config.inference).toEqual({ effort: "medium", speed: "standard" });
  expect(previewDraft.config.context_policy).toEqual({ kind: "keep_last", keep_last: 18 });
  expect(previewDraft.config.compaction).toEqual({ window: 32000, keep_recent: 8 });
  await checkpoint("an isolated Preview starts without saving the Agent", async () => {
    await expect(page.locator(".pill").filter({ hasText: /^AI SDK$/ })).toBeVisible();
    await expect(page.getByText(/Test the behavior you just configured|测试刚刚配置的行为/)).toBeVisible();
    const config = await page.request.get(`${BACKEND}/v1/config/agents/${AGENT_ID}`);
    expect(config.status()).toBe(404);
  });
  const ask = page.getByLabel(/^Message to agent$|^给 Agent 的消息$/);
  await type(ask, FIRST_TASK, { delay: 3 });
  await ask.press("Enter");
  await say("A removed field and renamed statuses must both be visible.", 2600);
  // Protocol Preview owns a visible conversation panel per transport. Scope the
  // result to the active AI SDK panel so hidden AG-UI history and other editor
  // surfaces cannot satisfy this runtime proof.
  const answer = page.locator('.agent-preview-conversation:not([hidden]) [data-role="assistant"]').last();
  await runtimeCheckpoint("the real model identifies both client-breaking contract changes", async () => {
    await expect(answer).toBeVisible({ timeout: 300_000 });
    // The answer may arrive before the AI SDK transport's terminal frame. Do not
    // present a partial response as complete; wait for the stream to settle.
    await expect(page.locator(".agent-working")).not.toBeVisible({ timeout: 300_000 });
    await expect(answer).toContainText(/Compatibility[\s\S]*(?:BREAKING|incompatible)/i, { timeout: 300_000 });
    await expect(answer).toContainText(/Breaking changes/i);
    await expect(answer).toContainText(/result_url/i);
    await expect(answer).toContainText(/status|enum|pending|completed/i);
    await expect(answer).toContainText(/not safe|do not publish|不可发布|不应发布/i);
  });
  await beat(
    "The result names both breaking changes and the migration work they require.",
    answer,
    3200,
  );

  await say("Now inspect the same Session through AG-UI.", 2200);
  await click(page.getByRole("button", { name: "AG-UI", exact: true }));
  const activeProtocolPreview = page.locator('.agent-preview-conversation:not([hidden])');
  const agUiComposer = page.getByLabel(/Message to agent over AG-UI|通过 AG-UI 给 Agent 的消息/);
  await checkpoint("AG-UI opens the same Preview history and its API key guide", async () => {
    await expect(agUiComposer).toBeVisible({ timeout: 60_000 });
    await expect(activeProtocolPreview.getByText(FIRST_TASK, { exact: true })).toBeVisible();
    await expect(activeProtocolPreview.locator('[data-role="assistant"]').first()).toContainText(/result_url/i);
    await expect(activeProtocolPreview.locator('[data-role="assistant"]').first()).toContainText(/status|pending|completed/i);
    await expect(page.getByRole("link", { name: /Connection and API key guide|连接与 API Key 指南/ }))
      .toHaveAttribute("href", /\/w\/default\/protocols#protocol-ag-ui$/);
  });
  const agUiAnswers = activeProtocolPreview.locator('[data-role="assistant"]');
  const priorAssistantCount = await agUiAnswers.count();
  await type(
    agUiComposer,
    "Using only the earlier result, restate the compatibility decision and both migration actions.",
    { delay: 3 },
  );
  await agUiComposer.press("Enter");
  // The shared history already contains the AI SDK answer. Wait for AG-UI to
  // append a new answer instead of letting `.last()` match that older result
  // while the second run is still in flight.
  const agUiAnswer = agUiAnswers.nth(priorAssistantCount);
  await runtimeCheckpoint("AG-UI preserves the decision and exposes its real lifecycle events", async () => {
    await expect(agUiAnswers).toHaveCount(priorAssistantCount + 1, { timeout: 300_000 });
    await expect(page.locator(".agent-working")).not.toBeVisible({ timeout: 300_000 });
    await expect(agUiAnswer).toContainText(/BREAKING|incompatible|not safe|do not publish|不可发布|不应发布/i);
    await expect(agUiAnswer).toContainText(/result_url|alias|mapping|migration note|映射|兼容别名|迁移说明/i);
    await expect(agUiAnswer).toContainText(/status|enum|new values|update clients|状态|枚举|更新客户端/i);
    await expect(page.locator(".agent-preview-event code").filter({ hasText: /^RUN_STARTED$/ })).toBeVisible({ timeout: 60_000 });
    await expect(page.locator(".agent-preview-event code").filter({ hasText: /^RUN_FINISHED$/ })).toBeVisible({ timeout: 60_000 });
  });
  await beat(
    "The decision stays in one Preview Session while AG-UI exposes its own lifecycle events.",
    page.locator(".agent-preview-events"),
    3200,
  );
  await click(page.getByRole("button", { name: /Close|关闭/ }));

  await say("Publish the reviewed draft as an immutable revision for new Sessions.", 2600);
  await click(page.getByRole("button", { name: /Review & publish|审阅并发布/, exact: true }));
  const publishModal = page.locator(".modal");
  let savedConfig;
  await checkpoint("the confirmation shows the exact reviewed Agent version", async () => {
    await expect(publishModal.getByText(/The draft passed all checks|草稿已通过所有检查/)).toBeVisible();
    await expect(publishModal.getByText(/Version used by new Sessions|新 Session 使用的版本/)).toBeVisible();
    savedConfig = await requireJson(await page.request.get(`${BACKEND}/v1/config/agents/${AGENT_ID}`), "Preview Agent readback");
    expect(savedConfig.model).toBe(MODEL_ID);
    expect(savedConfig.inference).toEqual({ effort: "medium", speed: "standard" });
    expect(savedConfig.context_policy).toEqual({ kind: "keep_last", keep_last: 18 });
    expect(savedConfig.compaction).toEqual({ window: 32000, keep_recent: 8 });
    expect(savedConfig.multiagent ?? null).toBeNull();
  });

  const isPublishRequest = (request) =>
    new URL(request.url()).pathname.endsWith(`/config/agents/${AGENT_ID}/publish`) &&
    request.method() === "POST";
  const publishRequest = page.waitForRequest(isPublishRequest, { timeout: 60_000 });
  const publishResponse = page.waitForResponse(
    (response) => isPublishRequest(response.request()),
    { timeout: 60_000 },
  );
  await click(publishModal.getByRole("button", { name: /Publish|发布/, exact: true }));
  await publishRequest;
  const publishedHttp = await publishResponse;
  expect(publishedHttp.ok()).toBeTruthy();
  const published = await publishedHttp.json();

  await checkpoint("the publication freezes the exact reviewed Agent revision", async () => {
    const publicationResponse = await page.request.get(
      `${BACKEND}/v1/config/publications/${published.fingerprint}`,
    );
    await requireOk(publicationResponse, "Preview publication readback");
    const publication = await publicationResponse.json();
    expect(publication.source_revision).toBe(savedConfig.generation);
    await expect(page.getByText(new RegExp(published.fingerprint.slice(0, 12)))).toBeVisible();
  });
  await clearCaption();
  await aha(story.aha, 4200);
  await wait(900);
  await clearCaption();
}
