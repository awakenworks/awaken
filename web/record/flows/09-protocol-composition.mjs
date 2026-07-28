// Protocol-composition proof: assemble a Managed Agents session from a portable
// Agent with direct MCP, an ACP environment, a Vault reference, and a session-only
// inline MCP server. There is no parallel Workspace MCP catalog.
import { configureSyntheticModel } from "../support/models.mjs";

const AGENT_ID = "protocol-portable-agent";
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
      metadata: {}, tools: [],
      mcp_servers: [{ type: "url", name: "docs-search", url: MCP_URL }],
      skills: [], max_steps: 8,
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

  await goto("/w/default/sessions");
  await intro(
    "Compose protocols without a second MCP configuration catalog.",
    "The Agent owns reusable direct MCP declarations; the session adds its ACP environment, Vault references, and one-off inline MCP.",
  );
  await say("The Agent already carries docs-search as name plus URL; the Vault supplies its matching credential without secrets on the Agent wire.", 4400);
  await click(page.getByRole("button", { name: /New session|新建会话/ }));
  const modal = page.locator(".modal");
  await say("The Managed Agents session chooses the Agent and ACP environment independently, then adds only session-specific MCP.", 4600);
  await modal.locator("select").nth(0).selectOption(AGENT_ID);
  await modal.locator("select").nth(1).selectOption(environment.id);
  await type(modal.getByPlaceholder("vlt_…"), vault.id);
  await click(modal.getByRole("button", { name: /add inline server|添加内联服务器/ }));
  await type(modal.getByPlaceholder("name"), "issue-tracker");
  await type(modal.getByPlaceholder("https://…"), "https://mcp.example.com/issues");

  await checkpoint("the session boundary visibly composes Agent, ACP runtime, Vault, and direct MCP", async () => {
    const agentResponse = await page.request.get(`http://127.0.0.1:38080/v1/config/agents/${AGENT_ID}`);
    expect(agentResponse.ok()).toBeTruthy();
    const agent = await agentResponse.json();
    expect(agent.mcp_servers).toEqual(expect.arrayContaining([
      expect.objectContaining({ name: "docs-search", url: MCP_URL }),
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
    await expect(modal).toContainText(/Agent MCP servers are included automatically|Agent 声明的 MCP 服务器会自动包含/);
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
