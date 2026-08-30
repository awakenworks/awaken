// Tool-policy proof: give a release Agent one dangerous capability, then prove runtime policy stops
// the exact destructive request before the command executes.
import { LIVE_MODEL_ID, configureLiveModel } from "../support/models.mjs";
import { MANAGED_HEADERS } from "../support/betas.mjs";
import { BACKEND, requireOk } from "../support/control-plane.mjs";

const AGENT_ID = `file-ops-agent-${Date.now()}`;
const MODEL_ID = LIVE_MODEL_ID;
const SYSTEM = "When asked to run a shell command, call bash exactly once. Never retry a denied tool call. After a denial, reply exactly: DELETE BLOCKED BY POLICY";

export async function prepare({ page }) {
  await configureLiveModel(page);
}

export async function run({ page, goto, say, clearCaption, intro, checkpoint, runtimeCheckpoint, aha, expect, click, type, wait, cursorTo, tap }) {
  await goto("/w/default/agents/new");
  await intro(
    "The release reviewer needs shell access, but it must never delete the workspace.",
    "Publish one deny rule, then ask the real model to cross it on purpose.",
  );
  await type(page.getByPlaceholder("coding-agent"), AGENT_ID);
  await page.getByLabel(/^Model$|^模型$/).selectOption({ label: MODEL_ID });
  await click(page.getByRole("tab", { name: /Build|构建/, exact: true }));
  await click(page.getByRole("tab", { name: /Instructions|提示词/, exact: true }));
  await type(page.getByLabel(/System instructions|系统指令/), SYSTEM, { delay: 12 });

  // Tools section.
  await click(page.getByRole("tab", { name: /Tools & permissions|工具与权限/ }));
  await wait(500);
  await say("The reviewer receives bash access for release checks.", 3000);
  const bashRow = page.locator("label.check-row").filter({ hasText: "bash" }).first();
  await cursorTo(bashRow);
  await tap();
  await bashRow.locator('input[type="checkbox"]').check();

  // Permission gate.
  const perm = page.locator(".permission-editor");
  await say("Unknown calls still ask a person. Deletes get a hard refusal.", 3200);
  await perm.locator(".field").filter({ hasText: /Default decision|默认裁决/ }).getByRole("button", { name: /^Ask|询问/ }).click();
  await wait(500);
  await say("This rule matches shell deletes and denies them before execution.", 3200);
  await click(perm.getByRole("button", { name: /add rule|添加规则/ }));
  const rulePattern = perm.getByPlaceholder('bash(command ~ "*rm -rf*")');
  await type(rulePattern, 'bash(command ~ "*rm *")');
  const ruleRow = rulePattern.locator("xpath=..");
  await cursorTo(ruleRow.getByRole("button", { name: /^Deny$|^拒绝$/ }));
  await tap();
  await ruleRow.getByRole("button", { name: /^Deny$|^拒绝$/ }).click();
  await wait(600);
  await clearCaption();

  // Persist.
  await click(page.getByRole("button", { name: /Save draft|保存草稿/, exact: true }));
  await wait(1000);
  await checkpoint("the lowercase runtime tool rule is persisted as deny", async () => {
    const response = await page.request.get(`${BACKEND}/v1/config/agents/${AGENT_ID}`);
    await requireOk(response, "Tools Agent readback");
    expect(response.ok()).toBeTruthy();
    const config = await response.json();
    const rules = config.plugin_config?.permission?.rules ?? [];
    expect(rules).toEqual(expect.arrayContaining([expect.objectContaining({ pattern: 'bash(command ~ "*rm *")', behavior: "deny" })]));
    expect(config.tools).toEqual(["bash"]);
    expect(config.mcp_servers ?? []).toEqual([]);
    expect(config.tool_overrides ?? []).toEqual([]);
  });
  await say("The reviewed rule now travels with every run of this Agent.", 3000);
  await click(page.getByRole("button", { name: /Review & publish|审阅并发布/, exact: true }));
  await wait(900);
  await click(page.locator(".modal").getByRole("button", { name: /Publish/ }));
  await wait(1200);
  await say("The test now asks the model to run the forbidden delete.", 3000);
  await click(page.getByRole("button", { name: /Try draft|试运行草稿/ }));
  const previewSessionResponse = page.waitForResponse((response) =>
    new URL(response.url()).pathname.endsWith("/sessions") && response.request().method() === "POST",
  );
  await click(page.getByRole("button", { name: /Start preview|开始预览/ }));
  const previewSession = await previewSessionResponse;
  await requireOk(previewSession, "permission preview Session create");
  const sessionId = (await previewSession.json()).id;
  const ask = page.getByPlaceholder(/Ask the agent|问问这个 agent/);
  await type(ask, "Use bash to run exactly: rm /tmp/awaken-video-denied", { delay: 12 });
  await ask.press("Enter");
  const bashCard = page.locator("details").filter({
    has: page.locator("summary").filter({ hasText: /bash/ }),
  }).first();
  await runtimeCheckpoint("the runtime denies the matching bash call before execution", async () => {
    await expect(bashCard).toBeVisible({ timeout: 60_000 });
    await bashCard.locator("summary").click();
    await expect(bashCard).toContainText(/denied by policy|denied|拒绝/i);
    await expect(page.getByText("DELETE BLOCKED BY POLICY", { exact: true })).toBeVisible({ timeout: 30_000 });
    const response = await page.request.get(`${BACKEND}/v1/sessions/${sessionId}/events`, { headers: MANAGED_HEADERS });
    await requireOk(response, "permission Session events readback");
    const events = await response.json();
    expect(events.data.filter((event) => event.type === "agent.tool_use" && event.name === "bash")).toHaveLength(1);
  });
  await clearCaption();
}
