// V — "Tools, presentation, and the permission gate." Configure what an agent CAN
// do and how it's allowed to do it: pick tools from the host catalog, rename/redescribe
// one for the model (tool presentation), then gate calls with a default decision + an
// ordered deny rule. Pure configurability — no code, enforced at runtime.
import { configureKimi } from "../support/models.mjs";

const AGENT_ID = "file-ops-agent";
const MODEL_ID = "kimi-for-coding";
const SYSTEM = "You are a tool-verification agent. When asked to run a shell command, call bash exactly once with that command.";

export const story = {
  promise: "Give an Agent useful native and MCP tools while proving a destructive shell request cannot execute.",
  effect: "The published permission rule visibly denies the model's matching bash call before execution.",
  aha: "The model can discover the tool and still cannot cross the runtime permission boundary.",
  loyalty: "Visible, reusable policy creates confidence to expand Agent capability without surrendering control.",
  satisfaction: "Users see the exact tool identity, presentation, and denial reason instead of debugging hidden policy.",
  advocacy: "A model attempting an action and the runtime stopping it is a crisp trust proof teams will share.",
};

export async function run({ page, goto, say, clearCaption, intro, checkpoint, runtimeCheckpoint, aha, expect, click, type, wait, cursorTo, tap }) {
  await configureKimi(page);
  await page.request.delete(`http://127.0.0.1:38080/v1/config/agents/${AGENT_ID}`).catch(() => {});

  await goto("/w/default/agents/new");
  await intro(
    "Give an agent useful tools without giving it unchecked authority.",
    "Separate capability, model-facing presentation, and runtime permission policy—all as versioned config.",
  );
  await type(page.getByPlaceholder("coding-agent"), AGENT_ID);
  await click(page.locator("select").first());
  await page.locator("select").first().selectOption(MODEL_ID);
  await type(page.locator("textarea").first(), SYSTEM, { delay: 12 });

  // Tools section.
  await click(page.getByRole("tab", { name: /Tools|工具/ }));
  await wait(500);
  await say("Pick executable tools from the host's advertised catalog.", 3200);
  for (const t of ["bash", "read", "write"]) {
    const row = page.locator("label.check-row").filter({ hasText: t }).first();
    await cursorTo(row);
    await tap();
    await row.locator('input[type="checkbox"]').check();
    await wait(350);
  }
  await wait(400);

  // Tool presentation: alias + description override.
  await say("Tool presentation: rename a tool for the model, or override its description.", 4200);
  await click(page.getByRole("button", { name: /override a selected tool|覆盖已选工具/ }));
  await wait(400);
  await type(page.getByPlaceholder("rename"), "run_shell");
  await type(page.getByPlaceholder("override description"), "Run a shell command in the sandbox.");
  await wait(400);

  await say("The same override mechanism targets MCP tools discovered after a server connects.", 3600);
  await click(page.getByRole("button", { name: /override an MCP tool|覆盖 MCP 工具/ }));
  await type(page.getByLabel("Canonical tool id 2"), "mcp__issues__create_issue", { delay: 10 });
  await type(page.getByPlaceholder("rename").nth(1), "file_issue");
  await page.getByLabel("Defer this tool").nth(1).check();
  await wait(500);

  // Permission gate.
  const perm = page.locator(".permission-editor");
  await say("Then gate every call. Default to Ask — a human approves before it runs.", 4200);
  await perm.locator(".field").filter({ hasText: /Default decision|默认裁决/ }).getByRole("button", { name: /^Ask|询问/ }).click();
  await wait(500);
  await say("And add a hard rule: shell deletes are always denied. Deny always wins.", 4200);
  await click(perm.getByRole("button", { name: /add rule|添加规则/ }));
  const rulePattern = perm.getByPlaceholder('bash(command ~ "*rm -rf*")');
  await type(rulePattern, 'bash(command ~ "*rm *")');
  const ruleRow = rulePattern.locator("xpath=..");
  await cursorTo(ruleRow.getByRole("button", { name: /^Deny$|^拒绝$/ }));
  await tap();
  await ruleRow.getByRole("button", { name: /^Deny$|^拒绝$/ }).click();
  await wait(600);
  await clearCaption();

  await click(page.getByRole("tab", { name: /Integrations|集成/, exact: true }));
  await say("Bind the direct MCP server separately; its tools stay dynamic instead of polluting the static catalog.", 4000);
  await click(page.getByRole("button", { name: /\+ MCP server|\+ MCP 服务器/, exact: true }));
  await type(page.getByLabel(/Server name|服务器名称/), "issues");
  await type(page.getByLabel("URL"), "https://mcp.example.com/issues", { delay: 10 });
  await wait(500);

  // Persist.
  await click(page.getByRole("button", { name: "Save", exact: true }));
  await wait(1000);
  await checkpoint("the lowercase runtime tool rule is persisted as deny", async () => {
    const response = await page.request.get(`http://127.0.0.1:38080/v1/config/agents/${AGENT_ID}`);
    expect(response.ok()).toBeTruthy();
    const config = await response.json();
    const rules = config.plugin_config?.permission?.rules ?? [];
    expect(rules).toEqual(expect.arrayContaining([expect.objectContaining({ pattern: 'bash(command ~ "*rm *")', behavior: "deny" })]));
    expect(config.tools).not.toContain("mcp__issues__create_issue");
    expect(config.mcp_servers).toEqual(expect.arrayContaining([expect.objectContaining({ name: "issues" })]));
    expect(config.tool_overrides).toEqual(expect.arrayContaining([
      expect.objectContaining({ target: "mcp__issues__create_issue", alias: "file_issue", defer: true }),
    ]));
  });
  await say("Save, then Publish — the gate compiles into the agent's runtime config.", 4000);
  await click(page.getByRole("button", { name: /Publish/ }));
  await wait(900);
  await click(page.locator(".modal").getByRole("button", { name: /Publish/ }));
  await wait(1200);
  await say("Now ask the live model to cross the exact boundary we just published.", 3400);
  await click(page.getByRole("button", { name: /Try it|试运行/ }));
  await click(page.getByRole("button", { name: /Start session|开始会话/ }));
  const ask = page.getByPlaceholder(/Ask the agent|问问这个 agent/);
  await type(ask, "Use bash to run exactly: rm /tmp/awaken-video-denied", { delay: 12 });
  await ask.press("Enter");
  const bashCard = page.locator("details").filter({ has: page.locator("code").filter({ hasText: /^bash$/ }) }).first();
  await runtimeCheckpoint("the runtime denies the matching bash call before execution", async () => {
    await expect(bashCard.getByText("error", { exact: true })).toBeVisible({ timeout: 60_000 });
    await bashCard.locator("summary").click();
    await expect(bashCard).toContainText(/denied by policy|denied|拒绝/i);
  });
  await aha(story.aha);
  await clearCaption();
}
