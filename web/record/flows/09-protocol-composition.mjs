// Protocol-composition proof: author a reusable MCP definition, then assemble a
// Managed Agents session from an Agent, an ACP environment, Vault references, and
// an inline MCP server without putting protocol-specific behavior in the Agent.

const AGENT_ID = "protocol-portable-agent";
const MCP_ID = "docs-search";
const MCP_URL = "https://mcp.example.com/docs";

export async function run({ page, goto, intro, say, clearCaption, checkpoint, aha, expect, click, type, wait }) {
  await page.request.put(`http://127.0.0.1:38080/v1/config/agents/${AGENT_ID}`, {
    data: {
      id: AGENT_ID,
      name: "Protocol-portable agent",
      model: { id: "" },
      system: "Use the resources attached to the session and cite the evidence you use.",
      metadata: {}, tools: [], mcp_servers: [], skills: [], max_steps: 8,
      plugins: [], plugin_config: {}, context_policy: { kind: "keep_all" },
    },
  });
  const envResponse = await page.request.post("http://127.0.0.1:38080/v1/environments", {
    data: { name: `Managed ACP · ${Date.now()}`, config: { type: "cloud", runtime: "acp:claude" } },
  });
  expect(envResponse.ok()).toBeTruthy();
  const environment = await envResponse.json();

  await goto("/w/default/mcp-servers");
  await intro(
    "Attach tools and execution protocols at the session boundary while keeping the Agent portable.",
    "Awaken composes Managed Agents sessions from an Agent, a Native or ACP environment, Vault references, and direct inline MCP.",
  );

  const author = page.locator("main").filter({ hasText: /Author MCP server|作者化 MCP 服务器/ });
  const fields = author.locator("input");
  await say("MCP works directly as name plus URL; the Vault supplies matching credentials without putting secrets on the wire.", 4400);
  await type(fields.nth(0), MCP_ID);
  await type(fields.nth(1), "Documentation search");
  await type(fields.nth(2), MCP_URL);
  await click(author.getByRole("button", { name: /Save|保存/, exact: true }));
  await wait(900);

  await goto("/w/default/sessions");
  await click(page.getByRole("button", { name: /New session|新建会话/ }));
  const modal = page.locator(".modal");
  await say("The Managed Agents session chooses the Agent and ACP environment independently, then merges project and inline MCP.", 4600);
  await modal.locator("select").nth(0).selectOption(AGENT_ID);
  await modal.locator("select").nth(1).selectOption(environment.id);
  await type(modal.getByPlaceholder("vlt_…"), "vlt_project_docs");
  await click(modal.getByRole("button", { name: /add inline server|添加内联服务器/ }));
  await type(modal.getByPlaceholder("name"), "issue-tracker");
  await type(modal.getByPlaceholder("https://…"), "https://mcp.example.com/issues");

  await checkpoint("the session boundary visibly composes Agent, ACP runtime, Vault, and direct MCP", async () => {
    const response = await page.request.get("http://127.0.0.1:38080/v1/config/mcp-servers");
    expect(response.ok()).toBeTruthy();
    const servers = await response.json();
    expect(servers).toEqual(expect.arrayContaining([
      expect.objectContaining({ id: MCP_ID, url: MCP_URL, credential_binding: { type: "none" } }),
    ]));
    await expect(modal.locator("select").nth(0)).toHaveValue(AGENT_ID);
    await expect(modal.locator("select").nth(1)).toHaveValue(environment.id);
    await expect(modal.getByPlaceholder("https://…")).toHaveValue("https://mcp.example.com/issues");
    await expect(modal).toContainText(/Project-bound MCP servers merge in automatically|项目绑定的 MCP 自动并入/);
  });

  await clearCaption();
  await aha("The Agent stays unchanged while Managed Agents, ACP, Vault, and MCP compose at the execution boundary.");
  await wait(1200);
  await clearCaption();
}
