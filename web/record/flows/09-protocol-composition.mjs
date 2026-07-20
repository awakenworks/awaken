// Protocol-composition proof: author a reusable MCP definition, then assemble a
// Managed Agents session from an Agent, an ACP environment, Vault references, and
// an inline MCP server without putting protocol-specific behavior in the Agent.
import { configureSyntheticModel } from "../support/models.mjs";

const AGENT_ID = "protocol-portable-agent";
const MCP_ID = "docs-search";
const MCP_URL = "https://mcp.example.com/docs";
const MODEL_ID = "protocol-recording-model";

export const story = {
  promise: "Assemble one Managed session from a portable Agent, ACP environment, Vault reference, and direct MCP tools.",
  effect: "Creating the session produces a visible acp:claude runtime while project and inline MCP remain boundary configuration.",
  aha: "The Agent stays unchanged while Managed Agents, ACP, Vault, and MCP compose at the execution boundary.",
  loyalty: "Composable protocols preserve reusable Agent definitions as infrastructure choices evolve.",
  satisfaction: "A single session form makes protocol ownership explicit and removes integration ambiguity.",
  advocacy: "The unchanged-Agent composition proof is a compact architecture story practitioners can share.",
};

export async function run({ page, goto, intro, say, clearCaption, checkpoint, aha, expect, click, type, wait }) {
  await configureSyntheticModel(page, MODEL_ID);
  await page.request.put(`http://127.0.0.1:38080/v1/config/agents/${AGENT_ID}`, {
    data: {
      id: AGENT_ID,
      name: "Protocol-portable agent",
      model: { id: MODEL_ID },
      system: "Use the resources attached to the session and cite the evidence you use.",
      metadata: {}, tools: [], mcp_servers: [], skills: [], max_steps: 8,
      plugins: [], plugin_config: {}, context_policy: { kind: "keep_all" },
    },
  });
  const publish = await page.request.post(`http://127.0.0.1:38080/v1/config/agents/${AGENT_ID}/publish`);
  expect(publish.ok()).toBeTruthy();
  const envResponse = await page.request.post("http://127.0.0.1:38080/v1/environments", {
    data: { name: `Managed ACP · ${Date.now()}`, config: { type: "cloud", runtime: "acp:claude" } },
  });
  expect(envResponse.ok()).toBeTruthy();
  const environment = await envResponse.json();
  const vaultResponse = await page.request.post("http://127.0.0.1:38080/v1/vaults", {
    data: { display_name: `Protocol docs · ${Date.now()}` },
  });
  expect(vaultResponse.ok()).toBeTruthy();
  const vault = await vaultResponse.json();
  const credentialResponse = await page.request.post(
    `http://127.0.0.1:38080/v1/vaults/${vault.id}/credentials`,
    {
      data: {
        type: "static_bearer",
        mcp_server_url: MCP_URL,
        token: "recording-only-secret", // awaken-allow: secret (synthetic recording fixture)
      },
    },
  );
  expect(credentialResponse.ok()).toBeTruthy();

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
  await type(modal.getByPlaceholder("vlt_…"), vault.id);
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
    const credentialsResponse = await page.request.get(
      `http://127.0.0.1:38080/v1/vaults/${vault.id}/credentials`,
    );
    expect(credentialsResponse.ok()).toBeTruthy();
    const credentials = await credentialsResponse.json();
    expect(credentials.data).toEqual(expect.arrayContaining([
      expect.objectContaining({ auth: expect.objectContaining({ type: "static_bearer", mcp_server_url: MCP_URL }) }),
    ]));
    await expect(modal.locator("select").nth(0)).toHaveValue(AGENT_ID);
    await expect(modal.locator("select").nth(1)).toHaveValue(environment.id);
    await expect(modal.getByPlaceholder("vlt_…")).toHaveValue(vault.id);
    await expect(modal.getByPlaceholder("https://…")).toHaveValue("https://mcp.example.com/issues");
    await expect(modal).toContainText(/Project-bound MCP servers merge in automatically|项目绑定的 MCP 自动并入/);
  });

  await say("Create the session: boundary configuration becomes visible runtime provenance, not Agent-specific code.", 4000);
  await click(modal.getByRole("button", { name: /Create|创建/, exact: true }));
  await checkpoint("the created Managed session carries the selected ACP environment", async () => {
    await expect(page).toHaveURL(/\/sessions\/[^/]+$/, { timeout: 15_000 });
    await expect(page.getByText("acp:claude", { exact: true })).toBeVisible({ timeout: 15_000 });
    const sessionId = page.url().split("/").at(-1);
    const response = await page.request.get(`http://127.0.0.1:38080/v1/sessions/${sessionId}`);
    expect(response.ok()).toBeTruthy();
    const session = await response.json();
    expect(session.agent.id).toBe(AGENT_ID);
    expect(session.environment_id).toBe(environment.id);
    expect(session.metadata["awaken.runtime"]).toBe("acp:claude");
  });

  await clearCaption();
  await aha(story.aha);
  await wait(1200);
  await clearCaption();
}
