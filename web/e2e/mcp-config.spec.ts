import { test, expect } from "@playwright/test";
import { randomBytes } from "node:crypto";
import { MANAGED_HEADERS } from "./betas";
import { workspaceApiPath, workspaceId } from "./workspace";
import { startTemplateMcp } from "../../e2e/fixtures/template_mcp_fixture.mjs";
import { startFakeAnthropic } from "../../e2e/fixtures/fake_anthropic_fixture.mjs";

const tokenA = randomBytes(24).toString("hex"), tokenB = randomBytes(24).toString("hex");
const marker = `mcp-ui-${randomBytes(6).toString("hex")}`;
const model = "mcp-ui-model";
let mcp: Awaited<ReturnType<typeof startTemplateMcp>>;
let upstream: Awaited<ReturnType<typeof startFakeAnthropic>>;
let credentialA: string, credentialB: string, vaultB: string;
let serverId: string;

test.beforeAll(async ({ request }) => {
  mcp = await startTemplateMcp({ tools: { accounts: ["read_source"] }, records: { accounts: [{ marker }] } }, { accounts: { [tokenA]: "Product", [tokenB]: "Personal" } });
  const providerKey = "mcp-ui-provider-key"; // awaken-allow: secret
  upstream = await startFakeAnthropic(providerKey, { models: [model], behavior: (req: { messages?: { content?: unknown[] }[]; tools?: { name: string }[] }) => {
    const blocks = (req.messages ?? []).flatMap((m) => Array.isArray(m.content) ? m.content : []) as { type?: string }[];
    const results = blocks.filter((b) => b.type === "tool_result");
    if (results.length) return { text: JSON.stringify(results.at(-1)) };
    const tool = req.tools?.find((t) => t.name.includes("read_source"));
    if (!tool) return { text: "No MCP tool available" };
    return { tool: { id: "ui-read", name: tool.name, input: {} } };
  } });
  const workspace = await workspaceId(request);
  const supplied = await request.post(await workspaceApiPath(request, "/v1/config/provider-connections"), { data: {
    workspace_id: workspace, idempotency_key: "mcp-ui-provider", provider_id: "anthropic", endpoint_name: "mcp-ui",
    display_name: "MCP UI test", dialect: "anthropic_messages", base_url: `${upstream.url}/v1`, secret: providerKey, timeout_secs: 30,
  } });
  expect(supplied.ok(), await supplied.text()).toBe(true);
  for (const [name, token, authType] of [["Product", tokenA, "static_bearer"], ["Personal", tokenB, "mcp_oauth"]]) {
    const created = await request.post(await workspaceApiPath(request, "/v1/vaults"), { headers: MANAGED_HEADERS, data: { display_name: `${name} UI Vault` } });
    expect(created.ok()).toBe(true); const vault = await created.json(); expect(vault.type).toBe("vault");
    const result = await request.post(await workspaceApiPath(request, `/v1/vaults/${vault.id}/credentials`), { headers: MANAGED_HEADERS, data: {
      display_name: `${name} Notion`, auth: { type: authType, mcp_server_url: mcp.servers[0].url, ...(authType === "mcp_oauth" ? { access_token: token } : { token }) },
    } });
    expect(result.ok(), await result.text()).toBe(true); const credential = await result.json();
    if (name === "Product") credentialA = credential.id; else { credentialB = credential.id; vaultB = vault.id; }
  }
});
test.afterAll(async () => { await mcp?.close(); await upstream?.close(); });

test("MCP catalog config selects an exact account and runs through the real UI", async ({ page, request }) => {
  // Graph: C1 local definition, C2 two matching credentials, C3 selected B,
  // C4 actual probe, C5 exact debug publication, C6 approval. U1 C1+C2 -> both
  // accounts; U2 C3+C4 -> B validation; U3 save/reload -> same default; U4 C5+C6
  // -> actual B tool invocation/marker; U5 Vault service grouping preserves both.
  test.setTimeout(90_000);
  serverId = `ui-notion-${Date.now()}`;
  await page.goto("/w/default/mcp");
  await page.getByRole("button", { name: "Add MCP server", exact: true }).click();
  const modal = page.getByRole("dialog");
  await modal.getByLabel("Server ID").fill(serverId);
  await modal.getByLabel("Display name").fill("Local Notion UI");
  await modal.getByLabel("Description").fill("Read account-specific source records.");
  await modal.getByLabel("MCP URL").fill(mcp.servers[0].url);
  await modal.getByRole("button", { name: "Save server" }).click();
  await expect(modal).not.toBeVisible();
  const accounts = page.getByLabel("Credential account");
  await expect(accounts.locator(`option[value="${credentialA}"]`)).toContainText("Product Notion");
  await expect(accounts.locator(`option[value="${credentialB}"]`)).toContainText("Personal Notion");
  await accounts.selectOption(credentialB);
  await page.getByRole("button", { name: "Check connection", exact: true }).click();
  const result = page.getByLabel("Connection test result");
  await expect(result).toContainText("valid"); await expect(result).toContainText("Personal Notion");
  await expect(result).toContainText("read_source");
  await page.screenshot({ path: test.info().outputPath("mcp-configuration.png"), fullPage: true });
  await page.getByRole("button", { name: "Save default credential" }).click();
  await expect(page.getByText("Default saved. Existing Agents and Sessions are unchanged.")).toBeVisible();
  await page.reload(); await page.getByRole("button", { name: /^Local Notion UI Read/ }).click();
  await expect(page.getByLabel("Credential account")).toHaveValue(credentialB);
  await page.getByRole("button", { name: "Test with an Agent" }).click();
  await page.getByLabel("Model", { exact: true }).selectOption(model);
  await page.getByRole("button", { name: "Start debug session" }).click();
  await expect(page.getByRole("link", { name: "Open full Session" })).toBeVisible();
  await page.getByRole("textbox", { name: "Message to agent" }).fill("Call read_source once and return its exact account and marker.");
  await page.getByRole("button", { name: "Send", exact: true }).click();
  await expect(page.getByRole("button", { name: "Allow", exact: true })).toBeVisible();
  const before = mcp.state.calls.length;
  await page.getByRole("button", { name: "Allow", exact: true }).click();
  await expect(page.getByLabel("Session conversation")).toContainText(marker);
  expect(mcp.state.calls.slice(before).some((c: { account: string }) => c.account === "Personal")).toBe(true);
  const listing = await request.get(await workspaceApiPath(request, "/v1/config/mcp-servers"));
  expect((await listing.json()).servers.find((s: { key: string }) => s.key === `local:${serverId}`).default_credential.vault_credential_id).toBe(credentialB);
  await page.goto("/w/default/vaults"); await page.getByRole("button", { name: "By MCP service" }).click();
  await expect(page.getByText(/Product Notion · static_bearer · Product UI Vault/)).toBeVisible();
  await expect(page.getByText(/Personal Notion · mcp_oauth · Personal UI Vault/)).toBeVisible();
  // U6 a selected catalog default seeds the Agent server plus first-run Vault;
  // shared-account opt-in stays false; switching editor stages cannot silently
  // turn that Session identity into an Agent-wide credential.
  await page.goto(`/w/default/agents/new?mcp=${encodeURIComponent(`local:${serverId}`)}`);
  await expect(page.getByRole("checkbox", { name: "Personal UI Vault" })).toBeChecked();
  await expect(page.getByRole("checkbox", { name: "Pin this shared account to the Agent for all future Sessions" })).not.toBeChecked();
  await page.getByRole("tab", { name: "Build", exact: true }).click();
  await page.getByRole("tab", { name: "Skills & MCP", exact: true }).click();
  await expect(page.getByLabel("Server name")).toHaveValue(serverId);
  await expect(page.getByLabel("Credential account").last()).toHaveValue("");
  expect(vaultB).toBeTruthy(); expect(credentialA).not.toBe(credentialB);
});

test("Vault names do not restrict authentication types", async ({ page, request }) => {
  // V1 historical category prefix + new credentials -> all official auth
  // types remain selectable; V2 public resource discriminator stays vault.
  const response=await request.post(await workspaceApiPath(request,"/v1/vaults"),{headers:MANAGED_HEADERS,data:{display_name:"Integrations · mixed types"}});
  expect(response.ok()).toBe(true);expect((await response.json()).type).toBe("vault");
  await page.goto("/w/default/vaults");
  const header=page.locator(".row").filter({has:page.getByText("Integrations · mixed types",{exact:true})});
  await header.getByRole("button",{name:"Add credential",exact:true}).click();
  const modal=page.getByRole("dialog");
  await expect(modal.getByRole("button",{name:/Environment variable/})).toBeVisible();
  await expect(modal.getByRole("button",{name:/MCP bearer token/})).toBeVisible();
  await expect(modal.getByRole("button",{name:/MCP OAuth/})).toBeVisible();
});
