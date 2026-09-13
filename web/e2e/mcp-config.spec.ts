import { test, expect, type Page, type APIRequestContext } from "@playwright/test";
import { readFileSync } from "node:fs";
import AxeBuilder from "@axe-core/playwright";
import { randomBytes } from "node:crypto";
import { MANAGED_HEADERS } from "./betas";
import { workspaceApiPath, workspaceId } from "./workspace";
import { startTemplateMcp } from "../../e2e/fixtures/template_mcp_fixture.mjs";
import { startFakeAnthropic } from "../../e2e/fixtures/fake_anthropic_fixture.mjs";

const tokenA = randomBytes(24).toString("hex"), tokenB = randomBytes(24).toString("hex");
const marker = `mcp-ui-${randomBytes(6).toString("hex")}`;
const runId = randomBytes(4).toString("hex");
const live = process.env.AWAKEN_UIUX_LIVE === "1";
const liveKey = live ? process.env.DEEPSEEK_API_KEY : undefined;
const model = liveKey ? "deepseek-flash" : `mcp-ui-model-${runId}`;
let mcp: Awaited<ReturnType<typeof startTemplateMcp>>;
let upstream: Awaited<ReturnType<typeof startFakeAnthropic>>;
let credentialA: string, credentialB: string, vaultB: string;
let serverId: string;

test.beforeAll(async ({ request }) => {
  if (live) expect(liveKey, "live acceptance requires a DeepSeek key; never fall back to a fixture").toBeTruthy();
  mcp = await startTemplateMcp({ tools: { accounts: ["read_source"] }, records: { accounts: [{ marker }] } }, { accounts: { [tokenA]: "Product", [tokenB]: "Personal" } });
  const providerKey = "mcp-ui-provider-key"; // awaken-allow: secret
  if (!liveKey) upstream = await startFakeAnthropic(providerKey, { models: [model], behavior: (req: { messages?: { content?: unknown[] }[]; tools?: { name: string }[] }) => {
    const blocks = (req.messages ?? []).flatMap((m) => Array.isArray(m.content) ? m.content : []) as { type?: string }[];
    const results = blocks.filter((b) => b.type === "tool_result");
    if (results.length) return { text: JSON.stringify(results.at(-1)) };
    const tool = req.tools?.find((t) => t.name.includes("read_source"));
    if (!tool) return { text: "No MCP tool available" };
    return { tool: { id: "ui-read", name: tool.name, input: {} } };
  } });
  const workspace = await workspaceId(request);
  const supplied = await request.post(await workspaceApiPath(request, "/v1/config/provider-connections"), { data: {
    workspace_id: workspace, idempotency_key: `mcp-ui-provider-${runId}`, provider_id: liveKey ? "deepseek" : "anthropic", endpoint_name: `mcp-ui-${runId}`,
    display_name: "MCP UI test", dialect: liveKey ? "open_ai_chat" : "anthropic_messages", base_url: liveKey ? "https://api.deepseek.com" : `${upstream.url}/v1`, secret: liveKey ?? providerKey, timeout_secs: 120,
  } });
  expect(supplied.ok(), await supplied.text()).toBe(true);
  for (const [name, token, authType] of [["Product", tokenA, "static_bearer"], ["Personal", tokenB, "mcp_oauth"]]) {
    const created = await request.post(await workspaceApiPath(request, "/v1/vaults"), { headers: MANAGED_HEADERS, data: { display_name: `${name} UI Vault ${runId}` } });
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
  test.setTimeout(liveKey ? 180_000 : 90_000);
  serverId = `ui-notion-${Date.now()}`;
  await page.goto("/w/default/mcp");
  await page.getByRole("button", { name: "Add MCP server", exact: true }).click();
  const modal = page.getByRole("dialog");
  await modal.getByLabel("Server ID").fill(serverId);
  await modal.getByLabel("Display name").fill(`Local Notion UI ${runId}`);
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
  await page.reload(); await page.getByRole("button", { name: new RegExp(`^Local Notion UI ${runId} Read`) }).click();
  await expect(page.getByLabel("Credential account")).toHaveValue(credentialB);
  await page.getByRole("button", { name: "Test with an Agent" }).click();
  await page.getByLabel("Model", { exact: true }).selectOption(model);
  // Live SSE must be admitted with the same beta as ordinary API calls;
  // a successful poll cannot conceal a permanently rejected subscription.
  const streaming = page.waitForResponse((response) => /\/events\/stream$/.test(new URL(response.url()).pathname));
  await page.getByRole("button", { name: "Start debug session" }).click();
  await expect(page.getByRole("link", { name: "Open full Session" })).toBeVisible();
  await page.getByRole("textbox", { name: "Message to agent" }).fill("Call read_source once and return its exact account and marker.");
  await page.getByRole("button", { name: "Send", exact: true }).click();
  expect((await streaming).status()).toBe(200);
  await expect(page.getByRole("button", { name: "Allow", exact: true })).toBeVisible({ timeout: 90_000 });
  const before = mcp.state.calls.length;
  await page.getByRole("button", { name: "Allow", exact: true }).click();
  await expect(page.getByLabel("Session conversation")).toContainText(marker, { timeout: 90_000 });
  expect(mcp.state.calls.slice(before).some((c: { account: string }) => c.account === "Personal")).toBe(true);
  // U7 internal spans/usage/confirmation receipts remain in the event log,
  // while chat presents messages, tool results and actionable approval only.
  await expect(page.getByLabel("Session conversation")).not.toContainText("[session.usage]");
  await expect(page.getByLabel("Session conversation")).not.toContainText("[span.model_request_start]");
  await expect(page.getByLabel("Session conversation")).not.toContainText("requires_action");
  await test.info().attach("execution-evidence", { body: JSON.stringify({ live, model, selected_account: "Personal", actual_calls: mcp.state.calls.slice(before), source_marker: marker }), contentType: "application/json" });
  await page.screenshot({ path: test.info().outputPath("mcp-debug-conversation.png"), fullPage: true });
  const listing = await request.get(await workspaceApiPath(request, "/v1/config/mcp-servers"));
  expect((await listing.json()).servers.find((s: { key: string }) => s.key === `local:${serverId}`).default_credential.vault_credential_id).toBe(credentialB);
  await page.goto("/w/default/vaults"); await page.getByRole("button", { name: "By MCP service" }).click();
  await expect(page.getByText(new RegExp(`Product Notion · static_bearer · Product UI Vault ${runId}`))).toBeVisible();
  await expect(page.getByText(new RegExp(`Personal Notion · mcp_oauth · Personal UI Vault ${runId}`))).toBeVisible();
  // U6 a selected catalog default seeds the Agent server plus first-run Vault;
  // shared-account opt-in stays false; switching editor stages cannot silently
  // turn that Session identity into an Agent-wide credential.
  await page.goto(`/w/default/agents/new?mcp=${encodeURIComponent(`local:${serverId}`)}`);
  await expect(page.getByRole("checkbox", { name: new RegExp(`Personal UI Vault.*${vaultB}`) })).toBeChecked();
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
  const response=await request.post(await workspaceApiPath(request,"/v1/vaults"),{headers:MANAGED_HEADERS,data:{display_name:`Integrations · mixed types ${runId}`}});
  expect(response.ok()).toBe(true);expect((await response.json()).type).toBe("vault");
  await page.goto("/w/default/vaults");
  const header=page.locator(".row").filter({has:page.getByText(`Integrations · mixed types ${runId}`,{exact:true})});
  await header.getByRole("button",{name:"Add credential",exact:true}).click();
  const modal=page.getByRole("dialog");
  await expect(modal.getByRole("button",{name:/Environment variable/})).toBeVisible();
  await expect(modal.getByRole("button",{name:/MCP bearer token/})).toBeVisible();
  await expect(modal.getByRole("button",{name:/MCP OAuth/})).toBeVisible();
});


async function configured(request: APIRequestContext, name = `UX ${randomBytes(5).toString("hex")}`, url = mcp.servers[0].url) {
  const id = `ux-${randomBytes(6).toString("hex")}`;
  const endpoint = await workspaceApiPath(request, "/v1/config/mcp-servers");
  const current = await (await request.get(endpoint)).json();
  const response = await request.put(endpoint, { data: { expected_revision: current.revision,
    definition: { id, name, description: "Configuration acceptance", icon: null, transport: { type: "url", url }, prompts_as_skills: false } } });
  expect(response.ok(), await response.text()).toBe(true);
  return { id, name, key: `local:${id}`, endpoint };
}
async function openServer(page: Page, server: { name: string }) {
  await page.goto("/w/default/mcp");
  await page.getByLabel("Search MCP servers").fill(server.name);
  await page.getByRole("button", { name: `${server.name} Configuration acceptance HTTP · local`, exact: true }).click();
}
async function noOverflow(page: Page) {
  await expect.poll(() => page.evaluate(() => document.documentElement.scrollWidth <= innerWidth + 1)).toBe(true);
}
async function accessibility(page: Page) {
  const result = await new AxeBuilder({ page }).withTags(["wcag2a", "wcag2aa", "wcag21aa"]).analyze();
  await test.info().attach("accessibility", { body: JSON.stringify(result.violations, null, 2), contentType: "application/json" });
  expect(result.violations.map((v) => ({ id: v.id, nodes: v.nodes.map((n) => n.target) }))).toEqual([]);
}

test("UX form keeps transport drafts, validates inputs, traps focus and confirms discard", async ({ page }) => {
  // F1 empty/invalid fields -> no save; F2 transport roundtrip -> values retained;
  // F3 Tab/Shift-Tab -> dialog containment; F4 Escape dirty -> stay/discard choice.
  await page.goto("/w/default/mcp");
  const add = page.getByRole("button", { name: "Add MCP server", exact: true });
  await add.click(); const dialog = page.getByRole("dialog");
  await expect(dialog.getByRole("button", { name: "Save server" })).toBeDisabled();
  await dialog.getByLabel("Server ID").fill("bad space"); await dialog.getByLabel("Display name").fill("Form UX");
  // Validate ID independently: an invalid URL must not mask a broken pattern
  // attribute (Chromium applies Unicode-v character-class syntax).
  await dialog.getByLabel("MCP URL").fill("https://fixture.test/mcp");
  expect(await dialog.getByLabel("Server ID").evaluate((input: HTMLInputElement) => input.validity.patternMismatch)).toBe(true);
  await dialog.getByLabel("MCP URL").fill("invalid");
  await dialog.getByRole("button", { name: "Save server" }).click();
  expect(await dialog.locator("form").evaluate((f: HTMLFormElement) => f.checkValidity())).toBe(false);
  await dialog.getByLabel("Server ID").fill("valid-ux"); await dialog.getByLabel("MCP URL").fill("https://fixture.test/mcp");
  await dialog.getByLabel("Transport", { exact: true }).selectOption("sandbox_stdio");
  await dialog.getByLabel("Sandbox command").fill("node"); await dialog.getByLabel("Arguments · one per line").fill("--flag\nscript.js");
  await dialog.getByLabel("Transport", { exact: true }).selectOption("url"); await expect(dialog.getByLabel("MCP URL")).toHaveValue("https://fixture.test/mcp");
  await dialog.getByLabel("Transport", { exact: true }).selectOption("sandbox_stdio"); await expect(dialog.getByLabel("Sandbox command")).toHaveValue("node");
  await expect(dialog.getByLabel("Arguments · one per line")).toHaveValue("--flag\nscript.js");
  await dialog.getByLabel("Transport", { exact: true }).focus();
  for (const key of ["Tab", "Shift+Tab"]) for (let n = 0; n < 14; n++) { await page.keyboard.press(key); await expect.poll(() => dialog.evaluate((d) => d.contains(document.activeElement))).toBe(true); }
  await accessibility(page);
  await page.keyboard.press("Escape"); await expect(page.getByText("Discard unsaved server changes?")).toBeVisible();
  await page.getByRole("button", { name: "Cancel", exact: true }).click(); await expect(dialog.getByLabel("Sandbox command")).toHaveValue("node");
  await dialog.getByRole("button", { name: "Close", exact: true }).click(); await page.getByRole("button", { name: "Discard", exact: true }).click();
  await expect(dialog).not.toBeVisible(); await expect(add).toBeFocused();
});

test("UX list and credential read failures recover without blank-success states", async ({ page, request }) => {
  // L1 failed list -> alert + reload, no editable unloaded form; L2 retry -> data;
  // L3 account failure -> disabled selector + retry; L4 recovery -> both accounts.
  const server = await configured(request); let listFail = true;
  await page.route("**/config/mcp-servers", async (route) => { if (listFail && route.request().method() === "GET") await route.abort("internetdisconnected"); else await route.continue(); });
  await page.goto("/w/default/mcp"); await expect(page.getByRole("alert")).toBeVisible();
  await expect(page.getByRole("button", { name: "Add MCP server", exact: true })).toBeDisabled();
  listFail = false; await page.getByRole("button", { name: "Reload configuration" }).click();
  await page.getByLabel("Search MCP servers").fill(server.name); await page.getByRole("button", { name: `${server.name} Configuration acceptance HTTP · local` }).click();
  let accountsFail = true;
  await page.route("**/config/mcp-servers/credentials?*", async (route) => { if (accountsFail) await route.fulfill({ status: 503, json: { error: { message: "Account service unavailable" } } }); else await route.continue(); });
  await page.reload(); await page.getByLabel("Search MCP servers").fill(server.name); await page.getByRole("button", { name: `${server.name} Configuration acceptance HTTP · local` }).click();
  await expect(page.getByRole("alert")).toContainText("Account service unavailable"); await expect(page.getByLabel("Credential account")).toBeDisabled();
  accountsFail = false; await page.getByRole("button", { name: "Retry accounts" }).click();
  await expect(page.getByLabel("Credential account")).toBeEnabled(); await expect(page.getByLabel("Credential account").locator(`option[value="${credentialB}"]`)).toBeAttached();
});

test("UX pending probe fixes identity and timeout retry clears the previous result", async ({ page, request }) => {
  // P1 in-flight check -> account fixed/no duplicate call; P2 timeout -> alert;
  // P3 explicit retry -> actual selected account; P4 changing account clears proof.
  const server = await configured(request); await openServer(page, server);
  await page.getByLabel("Credential account").selectOption(credentialB);
  let release!: () => void; const gate = new Promise<void>((r) => { release = r; }); let fault = true;
  await page.route("**/mcp-servers/*/inspect", async (route) => {
    if (fault) { await gate; await route.fulfill({ status: 504, json: { error: { message: "Connection timed out" } } }); } else await route.continue();
  });
  await page.getByRole("button", { name: "Check connection", exact: true }).click();
  await expect(page.getByRole("button", { name: "Checking…" })).toBeDisabled(); await expect(page.getByLabel("Credential account")).toBeDisabled();
  release(); await expect(page.getByRole("alert")).toContainText("Connection timed out");
  fault = false; await page.getByRole("button", { name: "Check connection", exact: true }).click();
  await expect(page.getByLabel("Connection test result")).toContainText("Personal Notion");
  await page.getByLabel("Credential account").selectOption(credentialA); await expect(page.getByLabel("Connection test result")).not.toBeVisible();
});

test("UX concurrent edit keeps draft, reloads revision and supports exact retry", async ({ page, request }) => {
  // C1 server changes after page read -> CAS conflict, draft retained;
  // C2 reload latest revision -> explicit retry succeeds; C3 remove cancel ->
  // definition retained; C4 confirm remove -> definition absent after reload.
  const server = await configured(request); await openServer(page, server);
  await page.getByRole("button", { name: "Edit server", exact: true }).click();
  await page.getByRole("dialog").getByLabel("Display name").fill("Edited retained draft");
  await configured(request, "Concurrent writer");
  await page.getByRole("dialog").getByRole("button", { name: "Save server" }).click();
  await expect(page.getByRole("dialog").getByRole("alert")).toBeVisible();
  await expect(page.getByRole("dialog").getByLabel("Display name")).toHaveValue("Edited retained draft");
  // Recovery remains accessible inside the dialog, not behind its focus trap.
  await page.getByRole("dialog").getByRole("button", { name: "Reload configuration" }).click();
  await page.getByRole("dialog").getByRole("button", { name: "Save server" }).click(); await expect(page.getByRole("dialog")).not.toBeVisible();
  await page.getByRole("button", { name: "Remove local definition" }).click(); await page.getByRole("button", { name: "Cancel", exact: true }).click();
  let data = await (await request.get(server.endpoint)).json(); expect(data.servers.some((s: { key: string }) => s.key === server.key)).toBe(true);
  await page.getByRole("button", { name: "Remove local definition" }).click(); await page.getByRole("button", { name: "Remove definition", exact: true }).click();
  await expect.poll(async () => (await (await request.get(server.endpoint)).json()).servers.some((s: { key: string }) => s.key === server.key)).toBe(false);
});

for (const [width, locale, theme] of [[360, "zh", "dark"], [768, "en", "light"], [1280, "en", "dark"], [640, "zh", "light"]] as const) {
  test(`UX responsive accessibility ${width} ${locale} ${theme}`, async ({ page, request }) => {
    // R1 phone/tablet/desktop/200%-equivalent 1280->640 reflow, both themes and
    // languages; long metadata -> no document overflow, dialog remains operable.
    const server = await configured(request, `Long ${randomBytes(3).toString("hex")}-${width}-${theme} ${"名称Account".repeat(12)}`, `https://example.test/${"long-path-".repeat(35)}`);
    await page.addInitScript(({ locale, theme }) => { localStorage.setItem("awaken.console.locale", locale); localStorage.setItem("awaken.console.theme", theme); }, { locale, theme });
    await page.setViewportSize({ width, height: 900 }); await page.goto("/w/default/mcp");
    await page.getByLabel(locale === "zh" ? "搜索 MCP 服务器" : "Search MCP servers").fill(server.name);
    await page.locator(".template-card").click(); await noOverflow(page); await accessibility(page);
    await page.screenshot({ path: test.info().outputPath(`mcp-${width}-${locale}-${theme}.png`), fullPage: true });
    await page.getByRole("button", { name: locale === "zh" ? "编辑服务器" : "Edit server", exact: true }).click();
    await expect(page.getByRole("dialog")).toBeVisible(); await noOverflow(page); await accessibility(page);
    await page.screenshot({ path: test.info().outputPath(`mcp-form-${width}-${locale}-${theme}.png`), fullPage: true });
    await page.getByRole("dialog").getByRole("button", { name: locale === "zh" ? "关闭" : "Close", exact: true }).click();
    await page.goto("/w/default/vaults"); await page.getByRole("button", { name: locale === "zh" ? "按 MCP 服务" : "By MCP service" }).click();
    await noOverflow(page); await accessibility(page);
  });
}

test("UX catalog failure keeps cache and source repair retries successfully", async ({ page, request }) => {
  // S1 failed sync -> visible failure + existing definitions stay; S2 repaired
  // source -> same standard sync succeeds; successful status is announced.
  const server = await configured(request); await openServer(page, server);
  await page.getByText("Catalog synchronization", { exact: true }).click();
  // A catalog consumer cannot be mistaken for a cloud publication service.
  // No positive publish action is rendered until that separate service exists.
  await expect(page.getByRole("button", { name: /Publish catalog/i })).toHaveCount(0);
  await page.getByLabel("Catalog source URL").fill("https://fixture.test/catalog.json");
  await page.getByRole("button", { name: "Save catalog source" }).click(); await expect(page.getByRole("status").filter({ hasText: "Catalog source saved." })).toBeVisible();
  let fault = true;
  await page.route("**/mcp-catalog/sync", async (route) => fault
    ? route.fulfill({ status: 503, json: { error: { message: "Catalog download failed" } } })
    : route.fulfill({ json: await (await request.get(server.endpoint)).json() }));
  await page.getByRole("button", { name: "Sync now" }).click(); await expect(page.getByRole("alert")).toContainText("Catalog download failed");
  await expect(page.getByRole("button", { name: `${server.name} Configuration acceptance HTTP · local` })).toBeVisible();
  fault = false; await page.getByRole("button", { name: "Sync now" }).click(); await expect(page.getByRole("status").filter({ hasText: "Catalog synchronized." })).toBeVisible();
});


test("UX archived default is explicit and never silently replaced by another account", async ({ page, request }) => {
  // A1 saved default becomes archived -> unavailable option remains explicit;
  // A2 inspect fails before MCP I/O; A3 selecting valid account -> real result.
  const server = await configured(request); const vault = await (await request.post(await workspaceApiPath(request, "/v1/vaults"), { headers: MANAGED_HEADERS, data: { display_name: "Retired UX Vault" } })).json();
  const credential = await (await request.post(await workspaceApiPath(request, `/v1/vaults/${vault.id}/credentials`), { headers: MANAGED_HEADERS, data: { display_name: "Retired account", auth: { type: "static_bearer", mcp_server_url: mcp.servers[0].url, token: tokenA } } })).json();
  const data = await (await request.get(server.endpoint)).json();
  expect((await request.put(`${server.endpoint}/${server.key}/default-credential`, { data: { expected_revision: data.revision, credential_id: credential.id } })).ok()).toBe(true);
  expect((await request.post(await workspaceApiPath(request, `/v1/vaults/${vault.id}/credentials/${credential.id}/archive`), { headers: MANAGED_HEADERS, data: {} })).ok()).toBe(true);
  await openServer(page, server); await expect(page.getByLabel("Credential account")).toHaveValue(credential.id);
  await expect(page.getByLabel("Credential account").locator("option:checked")).toContainText("Selected credential unavailable");
  const before = mcp.state.authentications.length;
  await page.getByRole("button", { name: "Check connection", exact: true }).click(); await expect(page.getByRole("alert")).toContainText("unavailable");
  expect(mcp.state.authentications.length).toBe(before);
  await page.getByLabel("Credential account").selectOption(credentialB); await page.getByRole("button", { name: "Check connection", exact: true }).click();
  await expect(page.getByLabel("Connection test result")).toContainText("Personal Notion");
});

test("UX missing models, forbidden writes and empty search give actionable feedback", async ({ page, request }) => {
  // E1 no model -> start disabled + route to model setup; E2 403 on save ->
  // retained form/permission message; E3 search zero -> explicit empty state.
  const server = await configured(request); await page.route("**/config/executable-models**", (route) => route.fulfill({ json: [] }));
  await openServer(page, server); await page.getByRole("button", { name: "Test with an Agent" }).click();
  await expect(page.getByRole("button", { name: "Start debug session" })).toBeDisabled(); await expect(page.getByRole("link", { name: "Configure a model" })).toBeVisible();
  await page.getByRole("button", { name: "Edit server", exact: true }).click(); await page.getByRole("dialog").getByLabel("Display name").fill("Forbidden retained");
  await page.route("**/config/mcp-servers", async (route) => route.request().method() === "PUT" ? route.fulfill({ status: 403, json: { error: { message: "Permission denied: credential write required" } } }) : route.continue());
  await page.getByRole("dialog").getByRole("button", { name: "Save server" }).click();
  await expect(page.getByRole("dialog").getByRole("alert")).toContainText("Permission denied"); await expect(page.getByRole("dialog").getByLabel("Display name")).toHaveValue("Forbidden retained");
  await page.getByRole("dialog").getByRole("button", { name: "Close", exact: true }).click(); await page.getByRole("button", { name: "Discard", exact: true }).click();
  await page.getByLabel("Search MCP servers").fill("does-not-exist-ever"); await expect(page.getByText("No servers match your search.")).toBeVisible();
});

test("UX debug retry reuses Session identity and denial prevents tool effects", async ({ page, request }) => {
  // D1 response lost after Session create -> show error; D2 explicit retry ->
  // same idempotency key and Session id, no second Session; D3 Deny -> zero call.
  test.setTimeout(120_000); const server = await configured(request); await openServer(page, server);
  await page.getByLabel("Credential account").selectOption(credentialB); await page.getByRole("button", { name: "Test with an Agent" }).click();
  await page.getByLabel("Model", { exact: true }).selectOption(model);
  let lost = false; const sessionIds: string[] = [], keys: string[] = [];
  await page.route("**/v1/**", async (route) => {
    if (!/\/sessions$/.test(new URL(route.request().url()).pathname) || route.request().method() !== "POST") return route.continue();
    keys.push(route.request().headers()["idempotency-key"]);
    const response = await route.fetch(); const body = await response.json(); sessionIds.push(body.id);
    if (!lost) { lost = true; await route.abort("connectionreset"); } else await route.fulfill({ response });
  });
  await page.getByRole("button", { name: "Start debug session" }).click(); await expect(page.getByRole("alert")).toBeVisible();
  await page.getByRole("button", { name: "Start debug session" }).click(); await expect(page.getByRole("link", { name: "Open full Session" })).toBeVisible();
  expect(keys[0]).toBeTruthy(); expect(new Set(keys).size).toBe(1); expect(new Set(sessionIds).size).toBe(1);
  const before = mcp.state.calls.length; await page.getByRole("textbox", { name: "Message to agent" }).fill("Call read_source once."); await page.getByRole("button", { name: "Send", exact: true }).click();
  await page.getByRole("button", { name: "Deny", exact: true }).click(); await expect(page.getByLabel("Session conversation")).toContainText("denied"); expect(mcp.state.calls.length).toBe(before);
  await page.getByRole("button", { name: "Interrupt debug run" }).click(); await expect(page.getByText("Interrupt submitted. Follow the Session events for settlement.")).toBeVisible();
});

test("UX saving a seeded new Agent preserves its first-run Vault selection", async ({ page, request }) => {
  // V1 configured default -> first run identity; V2 new->saved route transition
  // is the same Agent, so identity survives; switching Agent remains a reset.
  const server = await configured(request); const data = await (await request.get(server.endpoint)).json();
  await request.put(`${server.endpoint}/${server.key}/default-credential`, { data: { expected_revision: data.revision, credential_id: credentialB } });
  await page.goto(`/w/default/agents/new?mcp=${encodeURIComponent(server.key)}`);
  await expect(page.getByRole("checkbox", { name: new RegExp(`Personal UI Vault.*${vaultB}`) })).toBeChecked();
  await page.getByRole("tab", { name: "Build", exact: true }).click(); await page.getByRole("tab", { name: "Instructions", exact: true }).click();
  await page.getByLabel("System instructions").fill("Read the selected MCP source when requested.");
  await page.getByRole("button", { name: "Save draft", exact: true }).click(); await expect(page).not.toHaveURL(/agents\/new/);
  await page.getByRole("tab", { name: "Quickstart", exact: true }).click(); await expect(page.getByRole("checkbox", { name: new RegExp(`Personal UI Vault.*${vaultB}`) })).toBeChecked();
});


test("UX many servers search by name and URL remains responsive", async ({ page, request }) => {
  // B1 max-size presentation fixture -> bounded page layout; B2 exact name/URL
  // search -> one result; B3 clear -> all definitions. Rendering fixture only;
  // create/config persistence is proven by the other real-backend scenarios.
  const endpoint = await workspaceApiPath(request, "/v1/config/mcp-servers");
  const current = await (await request.get(endpoint)).json(); const sample = current.servers[0];
  const rows = Array.from({ length: 128 }, (_, i) => ({ ...sample, key: `local:volume-${i}`, origin: "local", definition: { ...sample.definition, id: `volume-${i}`, name: `Server ${String(i).padStart(3, "0")}`, transport: { type: "url", url: `https://volume-${i}.example.test/mcp` } } }));
  await page.route("**/config/mcp-servers", (route) => route.fulfill({ json: { ...current, servers: rows } }));
  await page.setViewportSize({ width: 360, height: 800 }); await page.goto("/w/default/mcp"); await expect(page.locator(".template-card")).toHaveCount(128);
  const started = Date.now(); await page.getByLabel("Search MCP servers").fill("Server 127"); await expect(page.locator(".template-card")).toHaveCount(1);
  const elapsed = Date.now() - started; await test.info().attach("search-latency-ms", { body: String(elapsed), contentType: "text/plain" }); expect(elapsed).toBeLessThan(2000);
  await page.getByLabel("Search MCP servers").fill("volume-37.example"); await expect(page.locator(".template-card")).toContainText("Server 037");
  await noOverflow(page); await page.getByLabel("Search MCP servers").fill(""); await expect(page.locator(".template-card")).toHaveCount(128);
});

test("UX stdio form persists arguments and makes credential limits explicit", async ({ page, request }) => {
  // T1 stdio authoring -> exact command/args survive reload; T2 HTTP probe/account
  // disabled, Session debug available; T3 no implicit credential injection claim.
  const id = `stdio-ux-${randomBytes(4).toString("hex")}`;
  await page.goto("/w/default/mcp"); await page.getByRole("button", { name: "Add MCP server", exact: true }).click();
  const dialog = page.getByRole("dialog"); await dialog.getByLabel("Server ID").fill(id); await dialog.getByLabel("Display name").fill(id);
  await dialog.getByLabel("Transport", { exact: true }).selectOption("sandbox_stdio"); await dialog.getByLabel("Sandbox command").fill("/usr/bin/node");
  await dialog.getByLabel("Arguments · one per line").fill("/mnt/session/uploads/server.js\n--readonly");
  await dialog.getByRole("button", { name: "Save server" }).click(); await expect(dialog).not.toBeVisible();
  await expect(page.getByLabel("Credential account")).toBeDisabled(); await expect(page.getByRole("button", { name: "Check connection", exact: true })).toBeDisabled();
  await expect(page.getByText(/This configuration does not inject Vault secrets/)).toBeVisible();
  const rows = await (await request.get(await workspaceApiPath(request, "/v1/config/mcp-servers"))).json();
  expect(rows.servers.find((s: { key: string }) => s.key === `local:${id}`).definition.transport).toEqual({ type: "sandbox_stdio", command: "/usr/bin/node", args: ["/mnt/session/uploads/server.js", "--readonly"] });
});


test("UX endpoint edits invalidate old default and allow explicit clearing", async ({ page, request }) => {
  // U1 target changes -> fingerprint mismatch notice + no old credential applied;
  // U2 clear default -> persisted null; reloading cannot restore the old identity.
  const server = await configured(request); let data = await (await request.get(server.endpoint)).json();
  await request.put(`${server.endpoint}/${server.key}/default-credential`, { data: { expected_revision: data.revision, credential_id: credentialB } });
  await openServer(page, server); await page.getByRole("button", { name: "Edit server", exact: true }).click();
  await page.getByRole("dialog").getByLabel("MCP URL").fill("https://unmatched.example.test/mcp");
  await page.getByRole("dialog").getByRole("button", { name: "Save server" }).click(); await expect(page.getByRole("dialog")).not.toBeVisible();
  await expect(page.getByText("The endpoint changed. Choose a matching credential again.")).toBeVisible();
  await expect(page.getByLabel("Credential account")).toHaveValue(""); await expect(page.getByText(/No matching accounts/)).toBeVisible();
  await page.getByRole("button", { name: "Save default credential" }).click(); await expect(page.getByText("Default saved. Existing Agents and Sessions are unchanged.")).toBeVisible();
  data = await (await request.get(server.endpoint)).json(); expect(data.servers.find((s: { key: string }) => s.key === server.key).default_credential).toBeNull();
});

test("UX model read failure is distinguished from no models and retry restores debugging", async ({ page, request }) => {
  // M1 model read error -> error+retry (not only empty); M2 retry -> actual ready
  // model and enabled start. Shared useModels remains the sole model query owner.
  const server = await configured(request); let fault = true;
  await page.route("**/config/executable-models**", async (route) => fault ? route.fulfill({ status: 503, json: { error: { message: "Models temporarily unavailable" } } }) : route.continue());
  await openServer(page, server); await page.getByRole("button", { name: "Test with an Agent" }).click();
  await expect(page.getByRole("alert")).toContainText("Models temporarily unavailable", { timeout: 20000 });
  fault = false; await page.getByRole("button", { name: "Retry models" }).click();
  await expect(page.getByRole("button", { name: "Start debug session" })).toBeEnabled();
});

test.describe("Touch acceptance", () => {
  test.use({ hasTouch: true, viewport: { width: 390, height: 844 } });
  test("UX touch controls use the shared touch density and the form scrolls to Save", async ({ page }) => {
    // H1 coarse pointer -> shared >=44px primary actions; H2 tall form -> body
    // scroll exposes Save and Close; no clipped or unreachable terminal action.
    await page.goto("/w/default/mcp"); const add = page.getByRole("button", { name: "Add MCP server", exact: true });
    expect(await add.evaluate((b) => b.getBoundingClientRect().height)).toBeGreaterThanOrEqual(44);
    await add.tap(); const dialog = page.getByRole("dialog");
    await dialog.getByLabel("Transport", { exact: true }).selectOption("sandbox_stdio");
    const save = dialog.getByRole("button", { name: "Save server" }); await save.scrollIntoViewIfNeeded(); await expect(save).toBeInViewport();
    await expect(dialog.getByRole("button", { name: "Close", exact: true })).toBeInViewport(); await noOverflow(page);
    await page.screenshot({ path: test.info().outputPath("mcp-touch-form.png"), fullPage: true });
  });
});


test("UX creates Vault and MCP credential entirely through the UI and discovers the account", async ({ page, request }) => {
  // K1 UI Vault create -> container; K2 write-only bearer entry -> canonical
  // auth payload and one child; K3 MCP selection discovers new account;
  // K4 another credential form never repopulates the previously entered secret.
  const server = await configured(request); const vaultName = `UI entry ${randomBytes(5).toString("hex")}`;
  const credentialName = `New browser account ${runId}`;
  const { header, modal } = await credentialEntry(page, vaultName); await modal.getByRole("button", { name: /MCP bearer token/ }).click();
  await modal.getByLabel("Credential name").fill(credentialName); await modal.getByLabel("MCP server URL").fill(mcp.servers[0].url);
  await modal.getByLabel("Secret (write-only)", { exact: true }).fill(tokenB);
  // K5 switching auth type or closing the draft clears both displayed and
  // parent-owned secret state; an empty field must never submit a hidden token.
  await modal.getByRole("button", { name: /MCP OAuth/ }).click();
  await expect(modal.getByLabel("Access token (write-only)", { exact: true })).toHaveValue("");
  await modal.getByRole("button", { name: /MCP bearer token/ }).click();
  await modal.getByLabel("MCP server URL").fill(mcp.servers[0].url);
  await modal.getByLabel("Secret (write-only)", { exact: true }).fill(tokenB);
  await modal.getByRole("button", { name: "Close", exact: true }).click();
  await header.getByRole("button", { name: "Add credential", exact: true }).click();
  await expect(modal.getByLabel("Secret (write-only)", { exact: true })).toHaveValue("");
  await expect(modal.getByRole("button", { name: "Save", exact: true })).toBeDisabled();
  await modal.getByLabel("Secret (write-only)", { exact: true }).fill(tokenB);
  const created = page.waitForRequest((r) => /\/vaults\/[^/]+\/credentials$/.test(new URL(r.url()).pathname) && r.method() === "POST");
  await modal.getByRole("button", { name: "Save", exact: true }).click();
  const payload = (await created).postDataJSON(); expect(payload.auth.type).toBe("static_bearer"); expect(payload.type).toBeUndefined();
  await expect(modal).not.toBeVisible();
  await header.getByRole("button", { name: "Add credential", exact: true }).click();
  await expect(modal.getByLabel("Secret (write-only)", { exact: true })).toHaveValue("");
  await modal.getByRole("button", { name: "Close", exact: true }).click();
  await openServer(page, server); await page.getByLabel("Credential account").selectOption({ label: `${credentialName} · ${vaultName} · static_bearer` });
  await page.getByRole("button", { name: "Check connection", exact: true }).click(); await expect(page.getByLabel("Connection test result")).toContainText(credentialName);
});


async function credentialEntry(page: Page, vaultName: string) {
  await page.goto("/w/default/vaults"); await page.getByLabel("Vault name", { exact: true }).fill(vaultName);
  await page.getByRole("button", { name: "Create Vault", exact: true }).click();
  const header = page.locator(".row").filter({ has: page.getByText(vaultName, { exact: true }) });
  await header.getByRole("button", { name: "Add credential", exact: true }).click();
  return { header, modal: page.getByRole("dialog") };
}
for (const auth of ["mcp_oauth", "environment_variable"] as const) {
  test(`UX ${auth} credential form submits the standard auth union`, async ({ page }) => {
    // K6 each auth branch -> standard {auth:{type,...},display_name} request;
    // OAuth refresh off -> no invented grant, ENV -> explicit allowed hosts.
    const { modal } = await credentialEntry(page, `Typed UI ${auth} ${runId}`);
    await modal.getByRole("button", { name: auth === "mcp_oauth" ? /MCP OAuth/ : /Environment variable/ }).click();
    await modal.getByLabel("Credential name").fill(`Typed ${auth}`);
    if (auth === "mcp_oauth") {
      // Existing-token authoring and refresh are supported. An OAuth login
      // authorization flow is absent and must not be presented as available.
      await expect(modal.getByRole("button", { name: /Authorize|Sign in|Connect account/i })).toHaveCount(0);
      await modal.getByLabel("MCP server URL").fill(mcp.servers[0].url);
      await modal.getByLabel("Access token (write-only)", { exact: true }).fill(tokenB);
      // K7 optional refresh off/on clears hidden refresh-token material and
      // incomplete grant fields keep Save disabled until repaired or disabled.
      const refresh = modal.getByRole("checkbox", { name: "Refresh this token automatically" });
      await refresh.check(); await modal.getByLabel("Client id", { exact: true }).fill("ui-client");
      await modal.getByLabel("Token endpoint", { exact: true }).fill("https://fixture.test/token");
      await modal.getByLabel("Refresh token", { exact: true }).fill("synthetic-refresh-draft");
      await refresh.uncheck(); await refresh.check(); await expect(modal.getByLabel("Refresh token", { exact: true })).toHaveValue("");
      await expect(modal.getByRole("button", { name: "Save", exact: true })).toBeDisabled(); await refresh.uncheck();
    } else {
      await modal.getByRole("textbox", { name: "Environment variable", exact: true }).fill("UX_TOKEN");
      await modal.getByLabel("Allowed hosts").fill("api.example.test");
      await modal.getByLabel("Secret (write-only)", { exact: true }).fill(tokenB);
    }
    const created = page.waitForRequest((r) => /\/vaults\/[^/]+\/credentials$/.test(new URL(r.url()).pathname) && r.method() === "POST");
    await modal.getByRole("button", { name: "Save", exact: true }).click();
    const payload = (await created).postDataJSON(); expect(payload.auth.type).toBe(auth); expect(payload.type).toBeUndefined();
    if (auth === "environment_variable") expect(payload.auth.networking).toEqual({ type: "limited", allowed_hosts: ["api.example.test"] });
    else expect(payload.auth.refresh).toBeUndefined();
    await expect(modal).not.toBeVisible(); expect(await page.locator("body").textContent()).not.toContain(tokenB);
  });
}


function readerLog() { return readFileSync(process.env.AWAKEN_ORCA_LOG!, "utf8"); }
async function heardAfter(cursor: number, expected: string) {
  // The oracle consumes actual Orca utterances, not DOM/AX text or an injected
  // speech mock. A per-action cursor excludes earlier successful utterances.
  await expect.poll(() => readerLog().slice(cursor).split("\n")
    .filter((line) => line.includes("SPEECH OUTPUT:")).join("\n"), { timeout: 15000 }).toContain(expected);
}

test.describe("Orca acceptance", () => {
  test.skip(!process.env.AWAKEN_ORCA_LOG, "requires the isolated real-reader runner");
  test("account, keyboard action and asynchronous result are spoken", async ({ page, request }) => {
    // O1 focused account -> name/value spoken; O2 keyboard Enter invokes real
    // MCP -> live status spoken; O3 HTTP error -> alert spoken without focus.
    const server = await configured(request); await openServer(page, server); await page.bringToFront();
    const account = page.getByLabel("Credential account"); await account.selectOption(credentialB);
    let cursor = readerLog().length; await page.getByLabel("Search MCP servers").focus(); await heardAfter(cursor, "Search MCP servers");
    cursor = readerLog().length; await account.focus(); await heardAfter(cursor, "Credential account"); await heardAfter(cursor, "Personal Notion");
    cursor = readerLog().length; await page.getByRole("button", { name: "Save default credential" }).focus();
    await heardAfter(cursor, "adding this server to an Agent");
    const check = page.getByRole("button", { name: "Check connection", exact: true });
    cursor = readerLog().length; await check.focus(); await heardAfter(cursor, "Check connection");
    cursor = readerLog().length; await page.keyboard.press("Enter"); await heardAfter(cursor, "valid");
    expect(await page.getByLabel("Connection test result").textContent()).toContain("Personal Notion");
    await page.route("**/mcp-servers/*/inspect", (route) => route.fulfill({ status: 504, json: { error: { message: "Connection timed out" } } }));
    cursor = readerLog().length; await check.click(); await heardAfter(cursor, "Connection timed out");
    await page.screenshot({ path: test.info().outputPath("orca-account-error.png"), fullPage: true });
  });
  test("Agent shared-account choice and Session Vault choice have spoken context", async ({ page, request }) => {
    // I1 Agent-wide pin vs Session-only Vault are different decisions; focusing
    // each native control must expose its label and explanatory scope to Orca.
    const server = await configured(request); let data = await (await request.get(server.endpoint)).json();
    await request.put(`${server.endpoint}/${server.key}/default-credential`, { data: { expected_revision: data.revision, credential_id: credentialB } });
    await page.goto(`/w/default/agents/new?mcp=${encodeURIComponent(server.key)}`); await page.bringToFront();
    const pin = page.getByRole("checkbox", { name: "Pin this shared account to the Agent for all future Sessions" });
    let cursor = readerLog().length; await pin.focus(); await heardAfter(cursor, "all future Sessions");
    const vault = page.getByRole("checkbox", { name: new RegExp(`Personal UI Vault.*${vaultB}`) });
    cursor = readerLog().length; await vault.focus(); await heardAfter(cursor, "Personal UI Vault");
    await heardAfter(cursor, "this Session");
    await page.screenshot({ path: test.info().outputPath("orca-agent-vault.png"), fullPage: true });
  });
  test("Chinese document language, required form controls and secret masking", async ({ page }) => {
    // C1 app language -> document lang (voice selection); C2 required field ->
    // localized name/requirement spoken; C3 password input never speaks its value.
    await page.goto("/w/default/mcp"); await page.bringToFront();
    await page.getByRole("button", { name: "Switch language" }).click();
    await expect(page.locator("html")).toHaveAttribute("lang", "zh-Hans");
    await page.getByRole("button", { name: "添加 MCP 服务器", exact: true }).click();
    const dialog = page.getByRole("dialog"); let cursor = readerLog().length;
    await dialog.getByLabel("服务器 ID", { exact: true }).focus(); await heardAfter(cursor, "服务器 ID"); await heardAfter(cursor, "required");
    await dialog.getByRole("button", { name: "关闭", exact: true }).click();
    await page.getByRole("button", { name: "切换语言" }).click();
    await expect(page.locator("html")).toHaveAttribute("lang", "en");
    const { modal } = await credentialEntry(page, `Reader secret ${runId}`);
    await modal.getByRole("button", { name: /MCP bearer token/ }).click();
    await modal.getByLabel("MCP server URL").fill("https://reader.example.test/mcp");
    const secret = modal.getByLabel("Secret (write-only)", { exact: true });
    cursor = readerLog().length; await secret.focus(); await heardAfter(cursor, "Secret");
    const marker = "synthetic-reader-hidden-value";
    await secret.fill(marker); await page.keyboard.press("Tab");
    await heardAfter(cursor, "Save");
    const spoken = readerLog().slice(cursor).split("\n").filter((line) => line.includes("SPEECH OUTPUT:"));
    expect(spoken.join("\n")).not.toContain(marker);
  });
});
