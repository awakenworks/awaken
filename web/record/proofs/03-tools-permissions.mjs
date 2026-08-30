// Tool-governance proof: give a release Agent one controlled capability, then prove
// runtime pauses the exact request until a person decides whether it may execute.
import { LIVE_MODEL_ID, configureLiveModel } from "../support/models.mjs";
import { MANAGED_HEADERS } from "../support/betas.mjs";
import { BACKEND, requireOk } from "../support/control-plane.mjs";

const AGENT_ID = `file-ops-agent-${Date.now()}`;
const MODEL_ID = LIVE_MODEL_ID;
const SYSTEM = "When asked to run a shell command, call bash exactly once. Never retry a denied tool call.";

export async function prepare({ page }) {
  await configureLiveModel(page);
}

export async function run({ page, goto, say, clearCaption, intro, checkpoint, runtimeCheckpoint, aha, expect, click, type, wait, cursorTo, tap }) {
  await goto("/w/default/agents/new");
  await intro(
    "The release reviewer needs shell access, while a person must retain execution authority.",
    "Publish one controlled typed ToolSet, then deny the real model's pending call.",
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

  // Governance cause/effect table: C1=bash is selected; C2=write/edit are not;
  // C3=the controlled preset is applied; C4=the real model requests bash.
  // E1=one typed ToolSet keeps bash enabled with `always_ask`; E2=unselected
  // controlled members stay disabled with `always_ask`; E3=the call is pending
  // without a result; E4=one explicit Deny produces only the matching blocked
  // result and a terminal end_turn, never the command's stdout marker.
  await say("Controlled modifications keep bash useful while reserving execution for a person.", 3200);
  await click(page.getByRole("button", { name: /Apply controlled modifications|应用受控修改/ }));
  await wait(600);
  await clearCaption();

  // Persist.
  await click(page.getByRole("button", { name: /Save draft|保存草稿/, exact: true }));
  await wait(1000);
  await checkpoint("the controlled typed ToolSet is persisted without legacy permission config", async () => {
    const response = await page.request.get(`${BACKEND}/v1/config/agents/${AGENT_ID}`);
    await requireOk(response, "Tools Agent readback");
    const config = await response.json();
    expect(config.plugin_config?.permission).toBeUndefined();
    expect(config.plugins ?? []).not.toContain("permission");
    const toolsets = config.tools.filter((tool) => tool.type === "agent_toolset_20260401");
    expect(toolsets).toHaveLength(1);
    expect(toolsets[0].configs).toEqual(expect.arrayContaining([
      expect.objectContaining({ name: "bash", enabled: true, permission_policy: { type: "always_ask" } }),
      expect.objectContaining({ name: "write", enabled: false, permission_policy: { type: "always_ask" } }),
      expect.objectContaining({ name: "edit", enabled: false, permission_policy: { type: "always_ask" } }),
    ]));
    expect(config.tools).not.toContain("bash");
    expect(config.mcp_servers ?? []).toEqual([]);
    expect(config.tool_overrides ?? []).toEqual([]);
  });
  await say("The reviewed approval policy now travels with every run of this Agent.", 3000);
  await click(page.getByRole("button", { name: /Review & publish|审阅并发布/, exact: true }));
  await wait(900);
  await click(page.locator(".modal").getByRole("button", { name: /Publish/ }));
  await wait(1200);
  await say("The test now asks the model to use bash; the person still owns execution.", 3000);
  await click(page.getByRole("button", { name: /Try draft|试运行草稿/ }));
  const previewSessionResponse = page.waitForResponse((response) =>
    new URL(response.url()).pathname.endsWith("/sessions") && response.request().method() === "POST",
  );
  await click(page.getByRole("button", { name: /Start preview|开始预览/ }));
  const previewSession = await previewSessionResponse;
  await requireOk(previewSession, "permission preview Session create");
  const sessionId = (await previewSession.json()).id;
  const ask = page.getByPlaceholder(/Ask the agent|问问这个 agent/);
  await type(ask, "Use bash to run exactly: printf AWAKEN_APPROVAL_PROOF", { delay: 12 });
  await ask.press("Enter");
  const bashCard = page.getByRole("region", { name: /Tool bash|工具 bash/, exact: true });
  let bashUseId;
  await runtimeCheckpoint("the runtime pauses the bash call awaiting explicit approval", async () => {
    await expect(bashCard).toBeVisible({ timeout: 60_000 });
    await expect(bashCard).toContainText(/awaiting approval|待确认/i);
    await expect(bashCard.getByRole("button", { name: /Deny|拒绝/, exact: true })).toBeVisible();
    const response = await page.request.get(`${BACKEND}/v1/sessions/${sessionId}/events`, { headers: MANAGED_HEADERS });
    await requireOk(response, "permission Session events readback");
    const events = await response.json();
    const uses = events.data.filter((event) => event.type === "agent.tool_use" && event.name === "bash");
    expect(uses).toHaveLength(1);
    expect(uses[0].evaluated_permission).toBe("ask");
    bashUseId = uses[0].id;
    expect(events.data.some((event) => event.type === "agent.tool_result" && event.tool_use_id === bashUseId)).toBeFalsy();
  });
  await click(bashCard.getByRole("button", { name: /Deny|拒绝/, exact: true }));
  await checkpoint("one explicit Deny blocks the pending bash call without executing printf", async () => {
    // Decision rules: D1 before Deny -> one ask-evaluated use and no result;
    // D2 after Deny -> one confirmation, one matching error result containing
    // the canonical blocked reason, no printf stdout marker, and end_turn.
    let settledEvents = [];
    await expect.poll(async () => {
      const response = await page.request.get(`${BACKEND}/v1/sessions/${sessionId}/events`, { headers: MANAGED_HEADERS });
      await requireOk(response, "permission denial readback");
      settledEvents = (await response.json()).data;
      const deniedResults = settledEvents.filter((event) =>
        event.type === "agent.tool_result" && event.tool_use_id === bashUseId);
      const latestIdle = settledEvents.findLast((event) => event.type === "session.status_idle");
      return {
        confirmations: settledEvents.filter((event) => event.type === "user.tool_confirmation"
          && event.tool_use_id === bashUseId && event.result === "deny").length,
        results: deniedResults.length,
        terminal: latestIdle?.stop_reason?.type,
      };
    }, { timeout: 30_000 }).toEqual({ confirmations: 1, results: 1, terminal: "end_turn" });
    expect(settledEvents.filter((event) =>
      event.type === "agent.tool_use" && event.name === "bash")).toHaveLength(1);
    const deniedResult = settledEvents.find((event) =>
      event.type === "agent.tool_result" && event.tool_use_id === bashUseId);
    expect(deniedResult?.is_error).toBe(true);
    const deniedText = (deniedResult?.content ?? [])
      .map((block) => block.type === "text" ? block.text : "")
      .join("\n");
    expect(deniedText).toMatch(/blocked: denied/i);
    expect(deniedText).not.toContain("AWAKEN_APPROVAL_PROOF");
  });
  await clearCaption();
}
