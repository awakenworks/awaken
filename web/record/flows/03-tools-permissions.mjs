// V — "Tools, presentation, and the permission gate." Configure what an agent CAN
// do and how it's allowed to do it: pick tools from the host catalog, rename/redescribe
// one for the model (tool presentation), then gate calls with a default decision + an
// ordered deny rule. Pure configurability — no code, enforced at runtime.

const AGENT_ID = "file-ops-agent";
const MODEL_ID = "policy-demo-model";
const SYSTEM = "You are a careful file-operations assistant. Prefer read-only actions.";

export async function run({ page, goto, say, clearCaption, intro, checkpoint, aha, expect, click, type, wait, cursorTo, tap }) {
  await page.request.delete(`http://127.0.0.1:38080/v1/config/agents/${AGENT_ID}`).catch(() => {});
  await page.request.put("http://127.0.0.1:38080/v1/config/providers/policy-demo", {
    data: { id: "policy-demo", slug: "policy-demo", display_name: "Policy demo", version: 1 },
  });
  await page.request.put("http://127.0.0.1:38080/v1/config/endpoints/policy-demo-ep", {
    data: { id: "policy-demo-ep", provider_id: "policy-demo", dialect: "anthropic_messages", base_url: "https://api.example.test/v1", timeout_secs: 60, display_name: "Policy demo", version: 1 },
  });
  await page.request.post("http://127.0.0.1:38080/v1/config/offerings", {
    data: { model_id: MODEL_ID, provider_id: "policy-demo", protocol_endpoint_id: "policy-demo-ep", dialect: "anthropic_messages", upstream_model: "demo" },
  });
  await page.request.post("http://127.0.0.1:38080/v1/config/credentials", {
    data: { workspace_id: "wrkspc_default", kind: "vault", provider_id: "policy-demo", secret: "video-demo-only" }, // awaken-allow: secret (synthetic recording fixture)
  });

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
  await aha("The same tool can be useful to the model, understandable by humans, and still denied at execution time.");
  await clearCaption();
}
