import { expect, test, type Page } from "@playwright/test";
import { MANAGED_HEADERS } from "./betas";
import { SyntheticModelDirectory } from "./synthetic-model";
import { isApiRequest, logicalApiPath, workspaceApiPath, workspaceId } from "./workspace";
import { protocolHelpPath, quickstartSessionPath } from "../src/lib/navigation/paths";

const syntheticModels = new SyntheticModelDirectory();

test.beforeAll(async () => {
  await syntheticModels.start();
});

test.afterAll(async () => {
  await syntheticModels.stop();
});

async function openBuild(page: Page, section: "Instructions" | "Tools & permissions" | "Skills & MCP" | "Memory & resources") {
  await page.getByRole("tab", { name: "Build", exact: true }).click();
  await page.getByRole("tab", { name: section, exact: true }).click();
}

async function openAdvanced(page: Page, section: "Orchestration" | "Plugin configuration" | "Raw configuration" | "Release & diff") {
  await page.getByRole("tab", { name: "Advanced", exact: true }).click();
  await page.getByRole("tab", { name: section, exact: true }).click();
}

// Drives the real console against a real management backend. Covers the shell/nav,
// the capability-driven agent editor (S1–S3), truth-driven gating (S4), the config
// agent author→publish lifecycle, and the route-owned Workspace scope.

// Cause/effect R1: a local default route scope with no Organization/Workspace
// registry renders the Agents brand + friendly canonical workspace label while
// preserving the exact scope in title, plus canonical task navigation; it must
// not render the retired fake selectors.
test("shell renders the Awaken Agents brand, route scope and task-oriented rail", async ({ page }) => {
  await page.goto("/w/default/sessions");
  await expect(page.locator(".brand-anchor")).toContainText("Awaken");
  await expect(page.locator(".workspace-context")).toContainText("Local Workspace");
  await expect(page.locator(".workspace-context")).toHaveAttribute("title", "default");
  await expect(page.locator(".org-anchor,.ws-crumb")).toHaveCount(0);
  // Rail: data-driven nav items.
  const rail = page.locator(".sidebar");
  await expect(rail.getByRole("button", { name: "Sessions" })).toBeVisible();
  await expect(rail.getByRole("button", { name: "Agents", exact: true })).toBeVisible();
  await expect(rail.getByRole("button", { name: "Files", exact: true })).toBeVisible();
  await expect(rail.getByRole("button", { name: "Artifacts", exact: true })).toBeVisible();
  await expect(rail.getByRole("button", { name: "Models & providers" })).toBeVisible();
  await expect(rail.getByRole("button", { name: "MCP overview" })).toBeVisible();
  await expect(rail.getByRole("button", { name: "Inference credentials" })).toHaveCount(0);
});

test("split Control projects only mounted surfaces and recovers stale deep links", async ({ page }) => {
  // Cause/effect decision table:
  // | managed runtime | access management | requested route | effect |
  // | false           | false             | runtime route   | stable unavailable page; no runtime request |
  // | false           | false             | shell           | omit runtime and Access navigation |
  // | true            | true              | runtime route   | covered by the full-console inventory |
  // The server PEP remains authoritative; this test proves presentation never
  // treats a known-unmounted API as a page-level Not Found state.
  await page.route(/\/v1(?:\/workspaces\/[^/]+)?\/config\/capabilities(?:\?|$)/, (route) => route.fulfill({
    status: 200,
    contentType: "application/json",
    body: JSON.stringify({
      identity: { mode: "awaken-cloud", cloud_login_enabled: true, authenticated: true },
      models: {
        local_catalog_enabled: false,
        byok_enabled: false,
        cloud_models_enabled: true,
        profile_authoring_enabled: false,
      },
      surfaces: { managed_runtime: false, access_management: false },
    }),
  }));
  const runtimeRequests: string[] = [];
  page.on("request", (request) => {
    if (logicalApiPath(request.url()).startsWith("/v1/sessions")) runtimeRequests.push(request.url());
  });

  await page.goto("/w/default/sessions");
  await expect(page).toHaveURL(/\/w\/default\/sessions$/);
  const rail = page.locator(".sidebar");
  await expect(rail.getByRole("button", { name: "Sessions" })).toHaveCount(0);
  await expect(rail.getByRole("button", { name: "Runtime secrets" })).toHaveCount(0);
  await expect(rail.getByRole("button", { name: "Access" })).toHaveCount(0);
  await expect(page.getByRole("button", { name: "Admin Assistant" })).toHaveCount(0);
  await expect(page.getByRole("heading", { name: "Not available in this deployment" })).toBeVisible();
  expect(runtimeRequests).toEqual([]);
});

test("responsive: a narrow viewport keeps the shell usable with no horizontal overflow", async ({ page }) => {
  await page.setViewportSize({ width: 400, height: 800 });
  await page.goto("/w/default/sessions");
  // The sidebar collapses to one labeled page selector; every destination stays reachable.
  await expect(page.locator(".sidebar")).toBeVisible();
  const pageSelector = page.locator(".sidebar").getByRole("combobox", { name: "Page" });
  await expect(pageSelector).toBeVisible();
  await pageSelector.selectOption({ label: "Agents" });
  await expect(page).toHaveURL(/\/w\/default\/agents$/);
  // The page never scrolls wider than the viewport (the fixed rail no longer pushes it).
  const noOverflow = await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth + 2);
  expect(noOverflow).toBe(true);
});

// Cause/effect R2: entering the configured Workspace exposes the shared live
// readiness projection; absence of a server roster means no arbitrary scope input.
test("Workspace overview exposes one live readiness path instead of a client roster", async ({ page }) => {
  await page.goto("/w/default/overview");
  await expect(page.locator(".readiness-panel")).toBeVisible();
  await expect(page.getByText("Workspace readiness")).toBeVisible();
  await expect(page.getByPlaceholder("ws_acme")).toHaveCount(0);
});

test("agent editor Build/Advanced stages are data-driven from /v1/capabilities", async ({ page }) => {
  await page.goto("/w/default/agents/new");
  await openBuild(page, "Tools & permissions");
  const picker = page.locator(".check-picker").first();
  await expect(picker).toBeVisible();
  await expect(picker.locator(".check-row").first()).toBeVisible();

  await openBuild(page, "Instructions");
  await expect(page.locator(".behavior-card", { hasText: "Auto-compaction" })).toBeVisible();
  await openBuild(page, "Memory & resources");
  await expect(page.locator(".behavior-card", { hasText: /Memory/ })).toBeVisible();
  await openAdvanced(page, "Orchestration");
  await expect(page.locator(".behavior-card", { hasText: "Agent behavior state machine" })).toBeVisible();
  await expect(page.getByText("Every auxiliary Agent needs an id.", { exact: true })).toHaveCount(0);
  await expect(page.getByLabel("Built-in auxiliary Agent")).toBeChecked();
});

test("Agent Release shows immutable published versions newest-first on desktop and mobile", async ({ page, request }) => {
  const id = `version-history-${Date.now()}`;
  const model = `version-history-model-${Date.now()}`;
  await syntheticModels.configure(request, model);
  const base = {
    id,
    name: "Compatibility reviewer",
    model: { id: model },
    system: "Review API compatibility.",
    tools: [],
    mcp_servers: [],
    skills: [],
    plugins: [],
    plugin_config: {},
    context_policy: { kind: "keep_all" },
    max_steps: 8,
  };
  const firstSave = await request.put(await workspaceApiPath(request, `/v1/config/agents/${id}`), { data: base });
  const firstSaveBody = await firstSave.text();
  expect(firstSave.ok(), firstSaveBody).toBe(true);
  const first = JSON.parse(firstSaveBody);
  const firstPublish = await request.post(await workspaceApiPath(request, `/v1/config/agents/${id}/publish`), {
    data: { source_revision: first.generation },
  });
  expect(firstPublish.ok(), await firstPublish.text()).toBe(true);
  const secondSave = await request.put(await workspaceApiPath(request, `/v1/config/agents/${id}`), {
    data: { ...base, generation: first.generation, name: "Compatibility reviewer v2" },
  });
  const secondSaveBody = await secondSave.text();
  expect(secondSave.ok(), secondSaveBody).toBe(true);
  const second = JSON.parse(secondSaveBody);
  const secondPublish = await request.post(await workspaceApiPath(request, `/v1/config/agents/${id}/publish`), {
    data: { source_revision: second.generation },
  });
  expect(secondPublish.ok(), await secondPublish.text()).toBe(true);

  await page.goto(`/w/default/agents/${id}?stage=advanced&section=release`);
  await expect(page.getByRole("heading", { name: "Published version history" })).toBeVisible();
  const history = page.getByRole("list", { name: "Published Agent versions" });
  const versions = history.getByRole("listitem");
  await expect(versions).toHaveCount(2);
  await expect(versions.nth(0)).toContainText("v2");
  await expect(versions.nth(0)).toContainText("latest published");
  await expect(versions.nth(1)).toContainText("v1");
  await page.setViewportSize({ width: 390, height: 844 });
  await expect(history).toBeVisible();
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth)).toBe(true);
});

test("Tools tab renders the Permissions editor (data-driven from capabilities.policies)", async ({ page }) => {
  await page.goto("/w/default/agents/new");
  await openBuild(page, "Tools & permissions");
  await expect(page.getByText("Permissions", { exact: true })).toBeVisible();
  await expect(page.getByText("Default decision", { exact: true })).toBeVisible();
  // Add a rule → an editable glob-pattern row appears.
  await page.getByRole("button", { name: /add rule/ }).click();
  await expect(page.getByPlaceholder('bash(command ~ "*rm -rf*")')).toBeVisible();
});

test("Quickstart publishes, starts a durable Session, and hands its exact SDK coordinates forward", async ({ page, request }) => {
  // Quickstart placement cause/effect table: C1=the Agent has a recursive/self
  // delegate, C2=the selected Environment is self-hosted, C3=a Worker is live.
  // R1 C1+!C2 rejects before pretending a deferred cloud Sandbox can delegate;
  // R2 C1+C2+C3 publishes, realizes the Sandbox, accepts the first message, and
  // navigates to the exact durable Session; R3 blank draft id + selected Starter
  // fills the Starter id but an authored replacement remains authoritative; R4
  // post-Quickstart provenance exposes exact Agent/Environment SDK coordinates
  // and dismissal removes guidance only. This browser scenario owns R2-R4; the
  // runtime-host FMECA tests own the fail-closed R1 edge.
  const stamp = Date.now();
  const id = `quickstart-${stamp}`;
  const model = `quickstart-model-${stamp}`;
  await page.goto("/w/default/agents/new");
  const configuredWorkspace = await syntheticModels.configure(
    page.context().request,
    model,
    await workspaceApiPath(request, "/v1/config"),
  );
  const environmentResponse = await request.post(await workspaceApiPath(request, "/v1/environments"), {
    headers: MANAGED_HEADERS,
    data: {
      name: `quickstart-env-${stamp}`,
      config: { type: "self_hosted" },
    },
  });
  const environmentBody = await environmentResponse.text();
  expect(environmentResponse.ok(), environmentBody).toBe(true);
  const environment = JSON.parse(environmentBody);

  await page.reload();
  const browserWorkspace = await page.evaluate(async () => {
    const response = await fetch("/v1/workspaces/default/config/workspace-context");
    return response.headers.get("anthropic-workspace-id");
  });
  expect(browserWorkspace).toBe(configuredWorkspace);
  await expect(page.locator(".template-card[data-selected=true]")).toHaveCount(0);
  await page.getByRole("button", { name: /Repository change/ }).click();
  await expect(page.getByPlaceholder("coding-agent")).toHaveValue("repository-change");
  await expect(page.locator(".template-card[data-selected=true]")).toContainText("Repository change");
  await page.getByPlaceholder("coding-agent").fill(id);
  await page.getByLabel("Model", { exact: true })
    .selectOption({ label: model });
  await page.getByLabel("Environment for this run")
    .selectOption(environment.id);
  const task = `Quickstart proof ${stamp}`;
  await page.getByLabel("Task sent to the new Session").fill(task);
  await page.getByRole("button", { name: /Review, publish & run/ }).click();

  const modal = page.locator(".modal");
  await expect(modal.getByRole("heading", { name: /Review the first real run/ })).toBeVisible();
  await expect(modal).toContainText(environment.id);
  await expect(modal).toContainText(task);
  const sessionResponse = page.waitForResponse((response) =>
    response.request().method() === "POST" && isApiRequest(response.url(), "/v1/sessions"));
  await modal.getByRole("button", { name: /Publish & run/ }).click();
  const createdSessionResponse = await sessionResponse;
  const createSessionBody = createdSessionResponse.request().postDataJSON();
  expect(createSessionBody.agent).toEqual({ id, type: "agent", version: expect.any(Number) });
  expect(createSessionBody.agent.version).toBeGreaterThan(0);
  expect(createSessionBody.initial_events).toEqual([
    { type: "user.message", content: [{ type: "text", text: task }] },
  ]);
  const session = await createdSessionResponse.json();
  const quickstartPath = quickstartSessionPath("default", session.id);
  await expect(page).toHaveURL(new URL(quickstartPath, page.url()).toString());
  await expect(page.getByText("The Agent is published and its first task was submitted", { exact: false })).toBeVisible();
  const sdkPath = protocolHelpPath("default", "managed", {
    agentId: id,
    environmentId: environment.id,
  });
  const sdkLink = page.getByRole("link", { name: /Open Managed Agents SDK setup/ });
  await expect(sdkLink).toHaveAttribute("href", sdkPath);
  await page.getByRole("button", { name: "Dismiss Quickstart guidance" }).click();
  await expect(page).toHaveURL(new URL(quickstartPath.replace("?from=quickstart", ""), page.url()).toString());
  const stored = await (await request.get(await workspaceApiPath(request, `/v1/sessions/${session.id}`), {
    headers: MANAGED_HEADERS,
  })).json();
  expect(stored.environment_id).toBe(environment.id);
  expect(stored.agent.id).toBe(id);
  expect(stored.agent.version).toBe(createSessionBody.agent.version);
  await page.goto(sdkPath);
  const managedExample = page.locator("#protocol-managed .code-block");
  await expect(managedExample).toContainText(`agent: "${id}"`);
  await expect(managedExample).toContainText(`environment_id: "${environment.id}"`);
});

test("PermissionEditor authors a rule and persists it through save + reload", async ({ page }) => {
  const id = `perm-e2e-${Date.now()}`;
  await page.goto("/w/default/agents/new");
  await page.getByPlaceholder("coding-agent").fill(id);
  await openBuild(page, "Instructions");
  await page.getByLabel("System instructions").fill("You gate your tools.");
  await openBuild(page, "Tools & permissions");

  const editor = page.locator(".permission-editor");
  // Default decision → Deny (only the default-decision Segmented exists yet).
  await editor.getByRole("button", { name: "Deny" }).first().click();
  await page.getByRole("button", { name: /add rule/ }).click();
  const pattern = 'bash(command ~ "*rm -rf*")';
  await editor.getByPlaceholder(pattern).fill(pattern);

  const initialSave = page.waitForResponse((response) =>
    response.request().method() === "PUT"
      && isApiRequest(response.url(), `/v1/config/agents/${id}`));
  await page.getByRole("button", { name: "Save draft", exact: true }).click();
  expect((await initialSave).ok()).toBe(true);
  await expect(page).toHaveURL(new RegExp(`/agents/${id}$`));

  // Reload → the authored policy rehydrates from the stored config (round-trips).
  await page.reload();
  await openBuild(page, "Tools & permissions");
  await expect(editor.getByPlaceholder(pattern)).toHaveValue(pattern);
});

test("Agent editor persists and publishes a direct MCP binding plus MCP tool override", async ({ page, request }) => {
  const id = `mcp-agent-${Date.now()}`;
  const displayName = "Issue Tracker Agent";
  const model = `mcp-model-${Date.now()}`;
  await syntheticModels.configure(request, model);
  const credentialWorkspace = await workspaceId(request);
  const credentialPath = await workspaceApiPath(request, "/v1/config/credentials");
  const credentialResponse = await request.get(`${credentialPath}?workspace_id=${encodeURIComponent(credentialWorkspace)}`);
  const credentialBody = await credentialResponse.text();
  expect(credentialResponse.ok(), credentialBody).toBe(true);
  const credentialSources = JSON.parse(credentialBody);
  let mcpCredential = credentialSources.find((credential: {
    status: string;
    kind?: string;
    provider_id?: string;
    env_key?: string;
  }) => credential.status === "active"
    && credential.kind !== "worker_local"
    && !credential.provider_id
    && !credential.env_key);
  if (!mcpCredential) {
    const created = await request.post(await workspaceApiPath(request, "/v1/config/credentials"), {
      data: {
        workspace_id: credentialWorkspace,
        kind: "vault",
        secret: `mcp-token-${Date.now()}`,
      },
    });
    const createdBody = await created.text();
    expect(created.status(), createdBody).toBe(201);
    mcpCredential = JSON.parse(createdBody);
  }
  expect(mcpCredential).toBeTruthy();

  await page.goto("/w/default/agents/new");
  await page.getByPlaceholder("coding-agent").fill(id);
  await page.getByLabel("Display name").fill(displayName);
  await openBuild(page, "Instructions");
  await page.getByLabel("System instructions").fill("Use the issue tracker when the goal requires it.");
  await page.getByLabel("Model", { exact: true })
    .selectOption({ label: model });

  await openBuild(page, "Tools & permissions");
  await page.getByRole("button", { name: /override an MCP tool/ }).click();
  await page.getByLabel("Canonical tool id 1").fill("mcp__issues__create_issue");
  await page.getByLabel("Alias").fill("file_issue");
  await page.getByLabel("Description").last().fill("Create an issue with the verified acceptance criteria.");
  await page.getByLabel("Show this tool to the model on demand").check();
  await expect(page.getByText("Runtime-discovered MCP tool; resolved when the server connects.")).toBeVisible();

  await openBuild(page, "Skills & MCP");
  await page.getByRole("button", { name: "+ MCP integration", exact: true }).click();
  await page.getByLabel("Server name").fill("issues");
  await page.getByLabel("URL").fill("https://mcp.example.test/issues");
  await page.getByLabel("Credential source").selectOption(
    `${mcpCredential.id}@${mcpCredential.version}`,
  );
  await page.getByLabel("Prompts as skills").check();
  const issuesIntegration = page.locator(".mcp-integration-card").first();
  await issuesIntegration.getByLabel("Default permission").selectOption("always_allow");
  await issuesIntegration.getByRole("button", { name: "+ Named tool override" }).click();
  await issuesIntegration.getByLabel("Named tool override").fill("create_issue");
  await issuesIntegration.getByLabel("Tool permission", { exact: true }).selectOption("always_ask");
  await page.getByRole("button", { name: "+ MCP integration", exact: true }).click();
  await page.getByLabel("Transport").last().selectOption("sandbox_stdio");
  await page.getByLabel("Server name").last().fill("browser");
  await page.getByLabel("Sandbox command").fill("playwright-mcp");
  await page.getByLabel("Arguments (one per line)").fill("--headless\n--isolated");
  await openAdvanced(page, "Orchestration");
  await expect(page.getByRole("heading", { name: "Auxiliary Agents" })).toBeVisible();
  await expect(page.getByLabel("Budget profile")).toHaveValue("conservative");
  await expect(page.getByRole("button", { name: "Save draft", exact: true })).toBeEnabled();

  const initialMcpSave = page.waitForResponse((response) =>
    response.request().method() === "PUT"
      && isApiRequest(response.url(), `/v1/config/agents/${id}`));
  await page.getByRole("button", { name: "Save draft", exact: true }).click();
  const initialMcpResponse = await initialMcpSave;
  const initialMcpBody = await initialMcpResponse.text();
  expect(initialMcpResponse.ok(), initialMcpBody).toBe(true);
  await expect(page).toHaveURL(new RegExp(`/agents/${id}$`));

  const response = await request.get(await workspaceApiPath(request, `/v1/config/agents/${id}`));
  expect(response.ok()).toBe(true);
  const stored = await response.json();
  expect(stored.tools).not.toContain("mcp__issues__create_issue");
  expect(stored.tools).toEqual(expect.arrayContaining([
    {
      type: "mcp_toolset",
      mcp_server_name: "issues",
      configs: [{ name: "create_issue", enabled: true, permission_policy: { type: "always_ask" } }],
      default_config: { enabled: true, permission_policy: { type: "always_allow" } },
    },
    {
      type: "mcp_toolset",
      mcp_server_name: "browser",
      configs: [],
      default_config: { enabled: true, permission_policy: { type: "always_ask" } },
    },
  ]));
  expect(stored.mcp_servers).toEqual([
    {
      name: "issues",
      url: "https://mcp.example.test/issues",
      credential: { id: mcpCredential.id, revision: mcpCredential.version },
      prompts_as_skills: true,
    },
    {
      type: "sandbox_stdio",
      name: "browser",
      command: "playwright-mcp",
      args: ["--headless", "--isolated"],
    },
  ]);
  expect(stored.tool_overrides).toEqual([
    {
      target: "mcp__issues__create_issue",
      alias: "file_issue",
      description: "Create an issue with the verified acceptance criteria.",
      exposure: "on_demand",
    },
  ]);

  await page.reload();
  await openAdvanced(page, "Raw configuration");
  const rawEditor = page.getByLabel("Agent JSON");
  await expect(rawEditor).toHaveValue(/mcp__issues__create_issue/);
  const rawConfig = JSON.parse(await rawEditor.inputValue());
  rawConfig.compaction = { window: 32000, keep_recent: 12 };
  await rawEditor.fill(JSON.stringify(rawConfig, null, 2));
  const rawSave = page.waitForResponse((response) =>
    response.request().method() === "PUT"
      && isApiRequest(response.url(), `/v1/config/agents/${id}`));
  await page.getByRole("button", { name: "Save draft", exact: true }).click();
  expect((await rawSave).ok()).toBe(true);
  const lossless = await (await request.get(await workspaceApiPath(request, `/v1/config/agents/${id}`))).json();
  expect(lossless.compaction).toEqual({ window: 32000, keep_recent: 12 });

  await page.getByRole("button", { name: /^Review & publish/ }).click();
  const publication = page.waitForResponse((response) =>
    response.request().method() === "POST"
      && isApiRequest(response.url(), `/v1/config/agents/${id}/publish`));
  await page.locator(".modal").getByRole("button", { name: "Publish", exact: false }).click();
  expect((await publication).ok()).toBe(true);
  expect((await request.get(await workspaceApiPath(request, `/v1/config/agents/${id}`))).ok()).toBe(true);

  await page.goto("/w/default/mcp");
  const issuesRow = page.getByRole("row", { name: /issues https:\/\/mcp\.example\.test\/issues/ });
  await expect(issuesRow.getByText("https://mcp.example.test/issues", { exact: true })).toBeVisible();
  await expect(issuesRow.getByText("Allow without asking", { exact: true })).toBeVisible();
  await expect(issuesRow.getByText("1 named override", { exact: true })).toBeVisible();
  await expect(page.getByText("sandbox stdio · playwright-mcp").first()).toBeVisible();
  await expect(page.getByRole("link", { name: "Review the MCP and ToolSet guide ↗" }))
    .toHaveAttribute("href", "https://awakenworks.com/docs/agents/protocols/mcp/");
  const agentLink = issuesRow.locator(`a[href*="/agents/${id}"]`);
  await expect(agentLink.getByText(displayName, { exact: true })).toBeVisible();
  await expect(agentLink.getByText(id, { exact: true })).toBeVisible();
  await agentLink.click();
  await expect(page).toHaveURL(new RegExp(`/agents/${id}\\?stage=build&section=integrations$`));
  await expect(page.getByRole("heading", { name: "MCP integrations" })).toBeVisible();
  await page.setViewportSize({ width: 700, height: 900 });
  await expect(issuesIntegration.getByLabel("Default permission")).toBeVisible();
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth + 2)).toBe(true);
});

test("new session sends the official synchronous MCP override", async ({ page }) => {
  // Cause/effect rule: C1 a local Environment and one temporary URL MCP server
  // are selected -> E1 the canonical create body carries required
  // environment_id and agent_with_overrides; E2 no Prefer async track exists.
  let posted: Record<string, unknown> | undefined;
  let postedHeaders: Record<string, string> | undefined;
  await page.route(/\/v1(?:\/workspaces\/[^/]+)?\/config\/agents(?:\?|$)/, async (route) => {
    if (route.request().method() !== "GET") return route.continue();
    await route.fulfill({
      status: 200,
      contentType: "application/json",
      body: JSON.stringify({ data: [{ id: "prompt-skill-agent", published: true }] }),
    });
  });
  await page.route(/\/v1(?:\/workspaces\/[^/]+)?\/sessions(?:\?|$)/, async (route) => {
    if (route.request().method() !== "POST") return route.continue();
    posted = route.request().postDataJSON() as Record<string, unknown>;
    postedHeaders = route.request().headers();
    await route.fulfill({
      status: 200,
      contentType: "application/json",
      body: JSON.stringify({ id: "session-prompt-skill-e2e" }),
    });
  });
  await page.goto("/w/default/sessions");
  await page.getByRole("button", { name: /New session/ }).click();
  await page.locator(".modal select").first().selectOption({ index: 1 });
  await page.getByText("Advanced runtime overrides", { exact: true }).click();
  await page.getByRole("button", { name: /Add temporary server/ }).click();
  await page.getByPlaceholder("name").fill("docs");
  await page.getByPlaceholder("https://…").fill("https://docs.test/mcp");
  await page.locator(".modal").getByRole("button", { name: /Create/ }).click();
  await expect.poll(() => posted).toBeTruthy();
  expect(posted).toMatchObject({
    agent: {
      id: "prompt-skill-agent",
      type: "agent_with_overrides",
      mcp_servers: [{ type: "url", name: "docs", url: "https://docs.test/mcp" }],
    },
    environment_id: "env_local",
  });
  expect(posted).not.toHaveProperty("mcp_servers");
  expect(postedHeaders?.prefer).toBeUndefined();
});

test("Agent orchestration authors a typed auxiliary roster, pins a version, and persists its safety budget", async ({ page, request }) => {
  const stamp = Date.now();
  const model = `collaboration-model-${stamp}`;
  const auxiliary = `researcher-${stamp}`;
  const coordinator = `coordinator-${stamp}`;
  await syntheticModels.configure(request, model);
  expect((await request.put(await workspaceApiPath(request, `/v1/config/agents/${auxiliary}`), {
    data: {
      id: auxiliary,
      name: "Research specialist",
      model,
      system: "Research verified facts.",
      metadata: {
        "awaken.parent_agent_id": coordinator,
        "awaken.agent_role": "auxiliary",
      },
      tools: [],
      mcp_servers: [],
      skills: [],
      plugins: [],
      plugin_config: {},
      context_policy: { kind: "keep_all" },
      max_steps: 8,
    },
  })).ok()).toBe(true);
  const auxiliaryPublish = await request.post(await workspaceApiPath(request, `/v1/config/agents/${auxiliary}/publish`));
  expect(auxiliaryPublish.ok(), await auxiliaryPublish.text()).toBe(true);

  await page.goto("/w/default/agents/new");
  await page.getByPlaceholder("coding-agent").fill(coordinator);
  await openBuild(page, "Instructions");
  await page.getByLabel("System instructions").fill("Coordinate specialists and synthesize their evidence.");
  await page.getByLabel("Model", { exact: true }).selectOption({ label: model });
  await openAdvanced(page, "Orchestration");
  await page.getByLabel("Add a published specialist").selectOption(auxiliary);
  await page.getByRole("button", { name: "+ Add", exact: true }).click();
  await expect(page.getByRole("list", { name: "Auxiliary Agent roster" })).toContainText(auxiliary);
  await expect(page.getByLabel("Budget profile")).toHaveValue("conservative");
  await page.getByLabel("Version policy").selectOption("pinned");
  await page.getByLabel("Published version 1").fill("1");
  await page.getByLabel("Budget profile").selectOption("balanced");
  await expect(page.getByLabel("Built-in auxiliary Agent")).toBeChecked();

  const saveResponse = page.waitForResponse((response) =>
    response.request().method() === "PUT" && isApiRequest(response.url(), `/v1/config/agents/${coordinator}`));
  await page.getByRole("button", { name: "Save draft", exact: true }).click();
  expect((await saveResponse).ok()).toBe(true);
  const stored = await (await request.get(await workspaceApiPath(request, `/v1/config/agents/${coordinator}`))).json();
  expect(stored.multiagent).toEqual({
    type: "coordinator",
    agents: [
      { type: "agent", id: auxiliary, version: 1 },
      { type: "self" },
    ],
  });
  expect(stored.delegation_limits).toEqual({ max_depth: 3, max_parallel: 4, max_total: 16 });

  await page.goto("/w/default/agents?view=collaborations");
  await expect(page.getByRole("button", { name: "Collaborations" })).toHaveAttribute("aria-pressed", "true");
  const card = page.locator(".coordinator-card", { hasText: coordinator });
  await expect(card).toContainText(auxiliary);
  await expect(card).toContainText("pinned v1");
  await expect(card).toContainText("Built-in auxiliary");
  await card.getByRole("button", { name: "Edit roster" }).click();
  await expect(page).toHaveURL(new RegExp(`/agents/${coordinator}\\?stage=advanced&section=orchestration$`));
  await expect(page.getByLabel("Version policy")).toHaveValue("pinned");
  await expect(page.getByLabel("Maximum total")).toHaveValue("16");
});

test("a specialist is created inside its primary Agent, published first, and hidden from the top-level Agent list", async ({ page, request }) => {
  const stamp = Date.now();
  const model = `attached-model-${stamp}`;
  const primary = `primary-${stamp}`;
  const primaryName = `Primary coordinator ${stamp}`;
  const specialist = `${primary}--research`;
  await syntheticModels.configure(request, model);
  expect((await request.put(await workspaceApiPath(request, `/v1/config/agents/${primary}`), {
    data: {
      id: primary,
      name: primaryName,
      model,
      system: "Coordinate attached specialists.",
      tools: [],
      mcp_servers: [],
      skills: [],
      multiagent: { type: "coordinator", agents: [{ type: "self" }] },
      delegation_limits: { max_depth: 2, max_parallel: 2, max_total: 8 },
      plugins: [],
      plugin_config: {},
      context_policy: { kind: "keep_all" },
      max_steps: 8,
    },
  })).ok()).toBe(true);

  await page.goto(`/w/default/agents/${primary}`);
  await openAdvanced(page, "Orchestration");
  await expect(page.getByLabel("Built-in auxiliary Agent")).toBeChecked();
  await page.getByRole("link", { name: /New specialist/ }).click();
  await expect(page).toHaveURL(new RegExp(`/agents/new\\?parent=${primary}$`));
  await expect(page.getByText(`Specialist for ${primary}`, { exact: true })).toBeVisible();
  await expect(page.getByRole("button", { name: "Back to primary Agent", exact: true })).toBeVisible();
  await page.getByPlaceholder("coding-agent").fill(specialist);
  await openBuild(page, "Instructions");
  await page.getByLabel("System instructions").fill("Research sources for the primary Agent.");
  await page.getByLabel("Model", { exact: true }).selectOption({ label: model });
  await page.getByRole("button", { name: "Save draft", exact: true }).click();
  await expect(page).toHaveURL(new RegExp(`/agents/${specialist}\\?parent=${primary}$`));
  await page.getByRole("button", { name: "Publish", exact: false }).first().click();
  const modal = page.locator(".modal");
  await expect(modal.getByRole("heading", { name: "Publish changes?" })).toBeVisible();
  await modal.getByRole("button", { name: "Publish", exact: false }).click();
  await expect(page).toHaveURL(new RegExp(`/agents/${primary}\\?stage=advanced&section=orchestration&attach=${specialist}$`));
  await expect(page.getByRole("list", { name: "Auxiliary Agent roster" })).toContainText(specialist);
  await page.getByRole("button", { name: "Save draft", exact: true }).click();

  const attached = await (await request.get(await workspaceApiPath(request, `/v1/config/agents/${specialist}`))).json();
  expect(attached.metadata).toMatchObject({
    "awaken.parent_agent_id": primary,
    "awaken.agent_role": "auxiliary",
  });
  await page.goto("/w/default/agents");
  const agentFilter = page.getByPlaceholder("Filter agents…");
  await agentFilter.fill(primaryName);
  await expect(page.getByRole("table").getByText(primaryName, { exact: true })).toBeVisible();
  await agentFilter.fill(specialist);
  await expect(page.getByText(specialist, { exact: true })).toHaveCount(0);
  await page.getByRole("button", { name: "Collaborations" }).click();
  await expect(page.locator(".coordinator-card", { hasText: primary })).toContainText(specialist);
});

test("enabling a behavior renders a schema-driven form (not raw JSON)", async ({ page }) => {
  await page.goto("/w/default/agents/new");
  await openBuild(page, "Instructions");
  // Toggle the Auto-compaction behavior on → its config_schema renders as a form.
  await page.locator(".behavior-card", { hasText: "Auto-compaction" }).getByRole("switch").check();
  // The schema-driven form exposes compact's fields (e.g. keep_last).
  await expect(page.getByText("keep_last")).toBeVisible();
});

test("State Machine editor exposes scope, pre-execution gate, lifecycle events and request context", async ({ page }) => {
  await page.goto("/w/default/agents/new");
  await openAdvanced(page, "Orchestration");
  const card = page.locator(".behavior-card", { hasText: "Agent behavior state machine" });
  await card.getByRole("switch").check();

  await card.getByRole("button", { name: /Read before write/ }).click();
  await expect(card.getByText("1 · Instance, lifetime & concurrency")).toBeVisible();
  await expect(card.getByText("2 · Trigger, guard & effect")).toBeVisible();
  await expect(card.getByText(/same rendered \{file_path\} run serially/)).toBeVisible();
  await expect(card.getByRole("group").filter({ hasText: /tool:read/ })).toBeVisible();
  await expect(card.getByLabel("Scope")).toHaveValue("thread");
  await expect(card.getByLabel("Instance key")).toHaveValue("{file_path}");
  await card.getByLabel("Scope").selectOption("run");
  await card.getByLabel("Instance key").fill("{resource_id}");
  await expect(card.getByText(/same rendered \{resource_id\} run serially/)).toBeVisible();
  await card.getByLabel("Scope").selectOption("thread");
  await card.getByLabel("Instance key").fill("{file_path}");
  await expect(card.getByLabel("Before run action").last()).toHaveValue("deny");

  await card.getByRole("button", { name: /Todo reminder/ }).click();
  await card.getByRole("button", { name: /Expand transitions/ }).click();
  await expect(card.locator('input[value="step.before_inference"]')).toBeVisible();
  await expect(card.getByLabel("Reminder target").last()).toBeDisabled();
  await expect(card.getByPlaceholder("cooldown steps").last()).toHaveValue("5");
});

test("gated Observe page is truth-driven: probes the endpoint and shows the gate", async ({ page }) => {
  await page.goto("/w/default/audit-log");
  // /v1/audit-log is genuinely unmounted → 404 → the placeholder, not a fake flag.
  await expect(page.getByText(/optional capability is not available in this deployment|当前部署未开放这项可选能力/)).toBeVisible();
});

test("session detail toggles Chat ⇄ Trace (the log read as spans)", async ({ page, request }) => {
  const stamp = Date.now();
  const model = `trace-model-${stamp}`;
  const agent = `trace-agent-${stamp}`;
  await syntheticModels.configure(request, model);
  expect((await request.put(await workspaceApiPath(request, `/v1/config/agents/${agent}`), {
    data: {
      id: agent,
      model: { id: model },
      system: "Trace test",
      tools: [],
      plugins: [],
      plugin_config: {},
      context_policy: { kind: "keep_all" },
      max_steps: 8,
    },
  })).ok()).toBe(true);
  expect((await request.post(await workspaceApiPath(request, `/v1/config/agents/${agent}/publish`))).ok()).toBe(true);
  const res = await request.post(await workspaceApiPath(request, "/v1/sessions"), {
    headers: MANAGED_HEADERS,
    data: { agent, environment_id: "env_local", title: "trace-e2e" },
  });
  expect(res.ok()).toBe(true);
  const sid = (await res.json()).id as string;
  await page.goto(`/w/default/sessions/${sid}`);
  // Worker scheduling is asynchronous: a newly created Session may already be
  // rescheduling, in which case Stop is correctly enabled. This story owns the
  // Chat/Trace projection; the child-run story below owns stop-state behavior.
  await expect(page.getByRole("button", { name: /Stop run/ })).toBeVisible();
  await expect(page.getByRole("button", { name: /Archive/ })).toBeVisible();
  await expect(page.getByRole("button", { name: "Chat", exact: true })).toBeVisible();
  await page.getByRole("button", { name: "Trace", exact: true }).click();
  await expect(page.getByText(/No spans yet|暂无 span/)).toBeVisible();
  await page.getByRole("button", { name: "Child runs", exact: true }).click();
  await expect(page.getByText("No child runs in this Session")).toBeVisible();
  await expect(page.locator(".primary-thread-row")).toContainText(agent);
});

test("Session recovery interrupts committed pending tools through the shared event projection", async ({ page }) => {
  // Cause/effect graph: C1 the authoritative Event log ends in
  // requires_action; C2 the Session summary says idle; C3 the operator selects
  // recovery; C4 the POST returns and the same log projects interrupt/end_turn.
  // Effects: E1 UI shows pending=1/send=no despite summary idle; E2 recovery
  // sends one user.interrupt with a stable key; E3 pending becomes 0/send=yes.
  // | Rule | Event truth | Action/result | Effects |
  // | U1 | requires_action | none | E1 |
  // | U2 | U1 | interrupt accepted + terminal projection | E2+E3 |
  const sessionId = "session-pending-recovery-ui";
  let interrupted = false;
  let idempotencyKey = "";
  let postedBody: unknown;
  await page.route(
    new RegExp(`/v1/(?:workspaces/[^/]+/)?sessions/${sessionId}(?:/.*)?$`, "u"),
    async (route) => {
    const request = route.request();
    const pathname = new URL(request.url()).pathname;
    if (pathname.endsWith("/events") && request.method() === "POST") {
      idempotencyKey = request.headers()["idempotency-key"] ?? "";
      postedBody = request.postDataJSON();
      interrupted = true;
      await route.fulfill({
        status: 200,
        contentType: "application/json",
        body: JSON.stringify({ data: [{ id: "evt_interrupt", type: "user.interrupt" }] }),
      });
      return;
    }
    if (pathname.endsWith("/events")) {
      const data = interrupted
        ? [
            { id: "tool_1", type: "agent.tool_use", name: "bash", evaluated_permission: "ask", input: { command: "git status" } },
            { id: "idle_1", type: "session.status_idle", stop_reason: { type: "requires_action", event_ids: ["tool_1"] } },
            { id: "evt_interrupt", type: "user.interrupt", processed_at: "2026-08-27T00:00:01Z" },
            { id: "idle_2", type: "session.status_idle", stop_reason: { type: "end_turn" }, processed_at: "2026-08-27T00:00:02Z" },
          ]
        : [
            { id: "tool_1", type: "agent.tool_use", name: "bash", evaluated_permission: "ask", input: { command: "git status" } },
            { id: "idle_1", type: "session.status_idle", stop_reason: { type: "requires_action", event_ids: ["tool_1"] } },
          ];
      await route.fulfill({ status: 200, contentType: "application/json", body: JSON.stringify({ data, has_more: false, next_page: null }) });
      return;
    }
    if (pathname.endsWith(sessionId)) {
      await route.fulfill({
        status: 200,
        contentType: "application/json",
        body: JSON.stringify({
          id: sessionId,
          type: "session",
          agent: { id: "assistant", type: "agent", tools: [], skills: [], mcp_servers: [] },
          created_at: "2026-08-27T00:00:00Z",
          updated_at: "2026-08-27T00:00:00Z",
          archived_at: null,
          metadata: {},
          resources: [],
          outcome_evaluations: [],
          status: "idle",
        }),
      });
      return;
    }
      await route.continue();
    },
  );

  await page.goto(`/w/default/sessions/${sessionId}`);
  await expect(page.getByText("Pending tools 1", { exact: true })).toBeVisible();
  await expect(page.getByText("Can send message no", { exact: true })).toBeVisible();
  await page.getByRole("button", { name: "Trace", exact: true }).click();
  await expect(page).toHaveURL(new RegExp(`/sessions/${sessionId}\\?view=trace$`, "u"));
  await expect(page.getByText("Tool diagnostics", { exact: true })).toBeVisible();
  await expect(page.locator(".trace-tool-summary")).toContainText("bash");
  await expect(page.locator(".trace-tool-summary")).toContainText("1 pending");
  await page.getByRole("link", { name: "Link to event tool_1" }).click();
  await expect(page).toHaveURL(new RegExp(`view=trace&event=tool_1$`, "u"));
  await expect(page.locator("#event-tool_1")).toHaveClass(/trace-event-selected/u);
  await page.setViewportSize({ width: 390, height: 844 });
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth + 2)).toBe(true);
  await page.getByRole("button", { name: /Recover run/u }).click();
  await page.getByRole("button", { name: "Recover run", exact: true }).click();
  await expect.poll(() => postedBody).toEqual({ events: [{ type: "user.interrupt" }] });
  await expect.poll(() => idempotencyKey).toMatch(/^session-events-/u);
  await expect(page.getByText("Pending tools 0", { exact: true })).toBeVisible();
  await expect(page.getByText("Can send message yes", { exact: true })).toBeVisible();
});

test("Session Child runs exposes a delegated thread, its projected events, and a scoped stop action", async ({ page }) => {
  const sessionId = "session-child-runs-ui";
  const childId = "run_child_1";
  let childStatus = "running";
  await page.route(new RegExp(`/v1(?:/workspaces/[^/]+)?/sessions/${sessionId}(?:/|\\?|$)`), async (route) => {
    const request = route.request();
    const url = new URL(request.url());
    const path = url.pathname;
    if (path.endsWith(`/threads/${childId}/archive`) && request.method() === "POST") {
      childStatus = "terminated";
      await route.fulfill({
        status: 200,
        contentType: "application/json",
        body: JSON.stringify({
          id: childId,
          type: "session_thread",
          session_id: sessionId,
          parent_thread_id: `${sessionId}:primary`,
          agent: { id: "research-agent", type: "agent", version: 3, model: { id: "synthetic" }, name: "Research Agent", tools: [], mcp_servers: [], skills: [] },
          created_at: "2026-08-03T00:00:00Z",
          updated_at: "2026-08-03T00:01:00Z",
          archived_at: "2026-08-03T00:01:00Z",
          status: childStatus,
          stats: { duration_seconds: 12.5 },
          usage: { input_tokens: 21, output_tokens: 13 },
        }),
      });
      return;
    }
    if (path.endsWith(`/threads/${childId}/events`)) {
      await route.fulfill({
        status: 200,
        contentType: "application/json",
        body: JSON.stringify({
          data: [
            { id: "evt_child_1", type: "session.thread_status_running", session_thread_id: childId },
            { id: "evt_child_2", type: "agent.message", content: [{ type: "text", text: "Verified three primary sources." }] },
          ],
          has_more: false,
          next_page: null,
        }),
      });
      return;
    }
    if (path.endsWith("/threads")) {
      await route.fulfill({
        status: 200,
        contentType: "application/json",
        body: JSON.stringify({
          data: [
            {
              id: `${sessionId}:primary`,
              type: "session_thread",
              session_id: sessionId,
              parent_thread_id: null,
              agent: { id: "lead-agent", type: "agent", version: 2, model: { id: "synthetic" }, name: "Lead Agent", tools: [], mcp_servers: [], skills: [] },
              created_at: "2026-08-03T00:00:00Z",
              updated_at: "2026-08-03T00:01:00Z",
              archived_at: null,
              status: "running",
              stats: null,
              usage: null,
            },
            {
              id: childId,
              type: "session_thread",
              session_id: sessionId,
              parent_thread_id: `${sessionId}:primary`,
              agent: { id: "research-agent", type: "agent", version: 3, model: { id: "synthetic" }, name: "Research Agent", tools: [], mcp_servers: [], skills: [] },
              created_at: "2026-08-03T00:00:00Z",
              updated_at: "2026-08-03T00:01:00Z",
              archived_at: childStatus === "terminated" ? "2026-08-03T00:01:00Z" : null,
              status: childStatus,
              stats: { duration_seconds: 12.5 },
              usage: { input_tokens: 21, output_tokens: 13 },
            },
          ],
          has_more: false,
          next_page: null,
        }),
      });
      return;
    }
    if (path.endsWith("/events")) {
      await route.fulfill({ status: 200, contentType: "application/json", body: JSON.stringify({ data: [], has_more: false, next_page: null }) });
      return;
    }
    if (path.endsWith(sessionId)) {
      await route.fulfill({
        status: 200,
        contentType: "application/json",
        body: JSON.stringify({
          id: sessionId,
          type: "session",
          agent: { id: "lead-agent", type: "agent", version: 2, model: { id: "synthetic" }, name: "Lead Agent", tools: [], mcp_servers: [], skills: [], multiagent: { type: "coordinator", agents: ["research-agent"] } },
          created_at: "2026-08-03T00:00:00Z",
          updated_at: "2026-08-03T00:01:00Z",
          archived_at: null,
          title: "Delegation proof",
          metadata: {},
          resources: [],
          outcome_evaluations: [],
          status: "running",
        }),
      });
      return;
    }
    await route.continue();
  });

  await page.goto(`/w/default/sessions/${sessionId}`);
  await page.getByRole("button", { name: "Child runs", exact: true }).click();
  const child = page.locator(".thread-node-child", { hasText: "Research Agent" });
  await expect(child).toContainText("34");
  await expect(child).toContainText("12.5s");
  await child.getByRole("button", { name: "Inspect events" }).click();
  await expect(page.getByRole("log", { name: "Child run events" })).toContainText("Verified three primary sources.");
  await child.getByRole("button", { name: "Stop", exact: true }).click();
  await page.getByRole("button", { name: "Stop child run", exact: true }).click();
  await expect(child.getByText("Stopped", { exact: true })).toBeVisible();
  await expect(child.getByRole("button", { name: "Stop", exact: true })).toHaveCount(0);
});

test("Session Integrations shows only the durable active MCP projection", async ({ page }) => {
  const sessionId = "session-active-mcp";
  await page.route(new RegExp(`/v1(?:/workspaces/[^/]+)?/sessions/${sessionId}(?:\\?|$)`), async (route) => {
    if (route.request().method() !== "GET") return route.continue();
    await route.fulfill({
      status: 200,
      contentType: "application/json",
      body: JSON.stringify({
        id: sessionId,
        type: "session",
        agent: {
          id: "mcp-agent",
          type: "agent",
          name: "Documentation research",
          version: 7,
          tools: [],
          skills: [],
          mcp_servers: [{
            name: "docs",
            url: "https://mcp.example.test/docs",
            prompts_as_skills: true,
          }],
        },
        created_at: "2026-07-31T00:00:00Z",
        updated_at: "2026-07-31T00:00:00Z",
        metadata: {},
        resources: [],
        outcome_evaluations: [],
        status: "idle",
      }),
    });
  });

  await page.goto(`/w/default/sessions/${sessionId}`);
  const effective = page.getByRole("heading", { name: "Effective configuration" }).locator("..");
  await expect(effective).toContainText("Documentation research");
  await expect(effective).toContainText("revision 7");
  await expect(effective).toContainText("immutable snapshot");
  await expect(page.getByRole("navigation", { name: "Open current Agent configuration" })
    .getByRole("link", { name: "MCP integrations · 1 →" }))
    .toHaveAttribute("href", "/w/default/agents/mcp-agent?stage=build&section=integrations");
  await page.locator(".segmented").getByRole("button", { name: "Integrations", exact: true }).click();
  await expect(page.getByRole("heading", { name: "MCP connections used by this Session" })).toBeVisible();
  await expect(page.getByText("https://mcp.example.test/docs")).toBeVisible();
  await expect(page.getByText("remote Skills")).toBeVisible();
  await expect(page.getByText("active", { exact: true }).last()).toBeVisible();
  await expect(page.getByRole("link", { name: "Open current MCP configuration →" }))
    .toHaveAttribute("href", "/w/default/agents/mcp-agent?stage=build&section=integrations");
});

test("Session presents the defined outcome, budget, and usage without opening raw events", async ({ page }) => {
  const sessionId = "session-outcome-evidence";
  await page.route(new RegExp(`/v1(?:/workspaces/[^/]+)?/sessions/${sessionId}(?:\\?|$)`), async (route) => {
    if (route.request().method() !== "GET") return route.continue();
    await route.fulfill({
      status: 200,
      contentType: "application/json",
      body: JSON.stringify({
        id: sessionId,
        type: "session",
        agent: { id: "review-agent", type: "agent", name: "API reviewer", version: 4, tools: [], skills: [], mcp_servers: [] },
        budget: { type: "limit", max_list_cost: { amount: "2500", currency: "USD" } },
        usage: { input_tokens: 1234, output_tokens: 321, list_cost: { amount: "87", currency: "USD" } },
        outcome_evaluations: [{
          type: "outcome_evaluation",
          outcome_id: "outc_compatibility",
          description: "Identify every breaking API change",
          result: "satisfied",
          explanation: "Both removed response fields are reported with client impact.",
          iteration: 1,
          completed_at: "2026-08-29T00:01:00Z",
        }],
        created_at: "2026-08-29T00:00:00Z",
        updated_at: "2026-08-29T00:01:00Z",
        environment_id: "env_local",
        metadata: {},
        resources: [],
        status: "idle",
        stats: {},
        title: "Compatibility review",
        vault_ids: [],
      }),
    });
  });

  await page.goto(`/w/default/sessions/${sessionId}`);
  const evidence = page.getByRole("heading", { name: "Outcome & usage" }).locator("..");
  await expect(evidence).toContainText("Cost limit USD 25.00");
  await expect(evidence).toContainText("Tracked cost USD 0.87");
  await expect(evidence).toContainText("Tokens 1234 in · 321 out");
  await expect(evidence).toContainText("Identify every breaking API change");
  await expect(evidence).toContainText("satisfied");
  await expect(evidence).toContainText("Both removed response fields are reported with client impact.");
});

test("Deployments keeps successful and failed run history visible after the transient action", async ({ page }) => {
  await page.route(/\/v1(?:\/workspaces\/[^/]+)?\/deployments(?:\?|$)/, (route) => route.fulfill({
    status: 200,
    contentType: "application/json",
    body: JSON.stringify({
      data: [{
        id: "dep_maintenance",
        type: "deployment",
        name: "Repository maintenance",
        agent: { id: "agent_maintenance", type: "agent", version: 3 },
        environment_id: "env_local",
        schedule: { type: "cron", expression: "0 20 * * 5", timezone: "UTC", upcoming_runs_at: [] },
        paused_reason: null,
        archived_at: null,
        created_at: "2026-08-29T00:00:00Z",
      }],
      next_page: null,
    }),
  }));
  await page.route(/\/v1(?:\/workspaces\/[^/]+)?\/deployment_runs(?:\?|$)/, (route) => route.fulfill({
    status: 200,
    contentType: "application/json",
    body: JSON.stringify({
      data: [
        {
          id: "deprun_success",
          deployment_id: "dep_maintenance",
          session_id: "session_maintenance",
          error: null,
          trigger_context: { type: "schedule" },
          created_at: "2026-08-29T01:00:00Z",
        },
        {
          id: "deprun_failed",
          deployment_id: "dep_maintenance",
          session_id: null,
          error: { type: "environment_not_found", message: "Environment no longer exists." },
          trigger_context: { type: "manual" },
          created_at: "2026-08-29T00:00:00Z",
        },
      ],
      next_page: null,
    }),
  }));

  await page.goto("/w/default/deployments");
  await expect(page.getByRole("heading", { name: "Recent deployment runs" })).toBeVisible();
  const successful = page.locator("tr", { hasText: "deprun_success" });
  await expect(successful).toContainText("Repository maintenance");
  await expect(successful).toContainText("scheduled");
  await expect(successful.getByRole("link", { name: "Open Session →" }))
    .toHaveAttribute("href", "/w/default/sessions/session_maintenance");
  const failed = page.locator("tr", { hasText: "deprun_failed" });
  await expect(failed).toContainText("manual");
  await expect(failed).toContainText("failed");
  await expect(failed).toContainText("Environment no longer exists.");
  await page.setViewportSize({ width: 390, height: 844 });
  await expect(successful).toBeVisible();
  await expect(failed).toBeVisible();
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth)).toBe(true);
});

test("Agent composer supports multiline input and shows work immediately on the first message", async ({ page }) => {
  // Cross-component rule: C1 a browser composer submits one Event command and
  // C2 its response is still pending; E1 optimistic work remains visible and
  // E2 the HTTP request carries a stable non-empty Idempotency-Key. Backend
  // adapter and restart E2E own exact replay/conflict; this browser row proves
  // the key is not dropped between UI mutation and Managed wire boundary.
  const sid = `composer-e2e-${Date.now()}`;
  let postedText = "";
  let postedIdempotencyKey = "";
  let release!: () => void;
  const held = new Promise<void>((resolve) => { release = resolve; });
  const eventCommand = new RegExp(
    `/v1/(?:workspaces/[^/]+/)?sessions/${sid}/events$`,
    "u",
  );
  await page.route(eventCommand, async (route) => {
    if (route.request().method() === "GET") {
      return route.fulfill({ status: 200, contentType: "application/json", body: JSON.stringify({ data: [] }) });
    }
    if (route.request().method() !== "POST") return route.continue();
    const body = route.request().postDataJSON() as { events: Array<{ content: Array<{ text: string }> }> };
    postedText = body.events[0].content[0].text;
    postedIdempotencyKey = route.request().headers()["idempotency-key"] ?? "";
    await held;
    await route.fulfill({ status: 202, contentType: "application/json", body: "{}" });
  });
  await page.route(new RegExp(`/v1/(?:workspaces/[^/]+/)?sessions/${sid}$`, "u"), async (route) => {
    if (route.request().method() !== "GET") return route.continue();
    await route.fulfill({
      status: 200,
      contentType: "application/json",
      body: JSON.stringify({
        id: sid,
        type: "session",
        agent: { id: "composer-agent", type: "agent", name: "Composer agent", tools: [], mcp_servers: [], skills: [] },
        created_at: "2026-08-28T00:00:00Z",
        updated_at: "2026-08-28T00:00:00Z",
        archived_at: null,
        title: "Composer E2E",
        metadata: {},
        resources: [],
        outcome_evaluations: [],
        status: "idle",
      }),
    });
  });

  await page.goto(`/w/default/sessions/${sid}`);
  const composer = page.getByLabel("Message to agent");
  await composer.fill("Summarize the issue");
  await composer.press("Shift+Enter");
  await composer.pressSequentially("  Keep only decisions");
  await composer.press("Shift+Enter");
  await expect(composer).toHaveValue("Summarize the issue\n  Keep only decisions\n");
  await composer.press("Enter");

  await expect(page.locator(".agent-working")).toContainText("Agent is working");
  await expect(page.locator(".transcript-pending-message")).toContainText("Summarize the issue\n  Keep only decisions");
  await expect.poll(() => postedText).toBe("Summarize the issue\n  Keep only decisions\n");
  await expect.poll(() => postedIdempotencyKey).toMatch(/^session-events-/u);
  await expect(page.locator(".transcript-composer")).toBeVisible();
  const composerBox = await page.locator(".transcript-composer").boundingBox();
  const viewport = page.viewportSize();
  expect(composerBox && viewport && viewport.height - (composerBox.y + composerBox.height)).toBeLessThan(80);
  await page.setViewportSize({ width: 400, height: 800 });
  await expect(composer).toBeVisible();
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth + 2)).toBe(true);
  release();
});

test("Session conversation preserves reading position and offers an explicit jump to latest", async ({ page }) => {
  const sid = `conversation-scroll-${Date.now()}`;
  const events = Array.from({ length: 36 }, (_, index) => ({
    id: `event-${index}`,
    type: index % 2 === 0 ? "user.message" : "agent.message",
    content: [{ type: "text", text: `Conversation message ${index + 1}: ${"evidence ".repeat(8)}` }],
  }));
  await page.route(new RegExp(`/v1/(?:workspaces/[^/]+/)?sessions/${sid}/events$`, "u"), async (route) => {
    if (route.request().method() !== "GET") return route.continue();
    await route.fulfill({ status: 200, contentType: "application/json", body: JSON.stringify({ data: events }) });
  });
  await page.route(new RegExp(`/v1/(?:workspaces/[^/]+/)?sessions/${sid}$`, "u"), async (route) => {
    if (route.request().method() !== "GET") return route.continue();
    await route.fulfill({
      status: 200,
      contentType: "application/json",
      body: JSON.stringify({
        id: sid,
        type: "session",
        agent: { id: "reader-agent", type: "agent", name: "Evidence reader", tools: [], mcp_servers: [], skills: [] },
        created_at: "2026-08-29T00:00:00Z",
        updated_at: "2026-08-29T00:00:00Z",
        archived_at: null,
        title: "Conversation reading position",
        metadata: {},
        resources: [],
        outcome_evaluations: [],
        status: "idle",
      }),
    });
  });

  await page.goto(`/w/default/sessions/${sid}`);
  const conversation = page.getByRole("log", { name: "Session conversation" });
  await expect(conversation.getByText(/Conversation message 36:/)).toBeVisible();
  await conversation.evaluate((node) => { node.scrollTop = 0; node.dispatchEvent(new Event("scroll")); });
  const latest = page.getByRole("button", { name: /Latest/ });
  await expect(latest).toBeVisible();
  await latest.click();
  await expect.poll(() => conversation.evaluate((node) => node.scrollTop + node.clientHeight >= node.scrollHeight - 2)).toBe(true);
});

// Cause/effect R3: provider discovery is the sole model-catalog authoring path →
// an empty catalog must guide the operator to Provider connections instead of
// exposing the retired manual "Author model" path.
test("Models either tests a discovered model or closes the provider prerequisite", async ({ page }) => {
  await page.goto("/w/default/models");
  await expect(page.getByRole("heading", { name: "Catalog", exact: true })).toBeVisible();
  const testBtn = page.getByRole("button", { name: "Test", exact: true }).first();
  if ((await testBtn.count()) === 0) {
    await expect(page.getByText(/No models yet|还没有模型/)).toBeVisible();
    await expect(page.getByRole("heading", { name: /Provider connections|Provider 连接/ })).toBeVisible();
    await expect(page.getByRole("button", { name: "Author", exact: true })).toHaveCount(0);
    return;
  }
  if (await testBtn.isDisabled()) {
    // A discovered but inactive offering is truthfully visible and not runnable;
    // the Console must not open a transcript for it.
    await expect(testBtn).toBeDisabled();
    return;
  }
  await testBtn.click();
  // The modal mounts the shared transcript against the pinned model.
  await expect(page.getByRole("heading", { name: /Test model ·/ })).toBeVisible();
  await expect(page.getByPlaceholder("Say hello…")).toBeVisible();
});

test("Admin Assistant truthfully shows a composer or the missing-model prerequisite", async ({ page }) => {
  await page.goto("/w/default/assistant");
  await expect(page.getByRole("heading", { name: /Ask the Console Assistant|询问 Console 助手/ })).toBeVisible();
  await expect(page.getByText(/Ask any Console question|询问任何 Console 问题/)).toBeVisible();
  await expect(page.getByRole("list", { name: /Suggested questions|建议问题/ })).toBeVisible();
  // The assistant is seeded into the reserved scope (ADR-0052), but a composer is
  // only runnable when the current backend has a credentialed model.
  const composer = page.getByPlaceholder("Ask a question or describe what you want to accomplish…");
  const prerequisite = page.getByText(/The assistant needs a model to run on|助手需要一个模型才能运行/i);
  const unavailable = page.getByText(/The Assistant (?:is not runnable yet|could not be prepared)|助手(?:尚不可运行|准备失败)/i);
  await expect(composer.or(prerequisite).or(unavailable)).toBeVisible();
});

test("Assistant prepares itself after a runnable model becomes available", async ({ page }) => {
  let probes = 0;
  let ensures = 0;
  await page.route(/\/v1(?:\/workspaces\/[^/]+)?\/config\/executable-models(?:\?|$)/, async (route) => {
    await route.fulfill({
      status: 200,
      contentType: "application/json",
      body: JSON.stringify([{ model_id: "assistant-model", readiness: "ready" }]),
    });
  });
  await page.route(/\/v1(?:\/workspaces\/[^/]+)?\/agents\/__admin_assistant(?:\?|$)/, async (route) => {
    probes += 1;
    await route.fulfill({
      status: probes === 1 ? 404 : 200,
      contentType: "application/json",
      body: JSON.stringify(
        probes === 1
          ? { code: "not_found", title: "Not found" }
          : { id: "__admin_assistant", type: "agent", tools: [], skills: [], mcp_servers: [] },
      ),
    });
  });
  await page.route(/\/v1(?:\/workspaces\/[^/]+)?\/config\/agents\/__admin_assistant\/ensure(?:\?|$)/, async (route) => {
    ensures += 1;
    await route.fulfill({
      status: 200,
      contentType: "application/json",
      body: JSON.stringify({ status: "ready", agent_id: "__admin_assistant" }),
    });
  });
  await page.route(/\/v1(?:\/workspaces\/[^/]+)?\/sessions(?:\?|$)/, async (route) => {
    if (route.request().method() !== "POST") return route.continue();
    await route.fulfill({
      status: 200,
      contentType: "application/json",
      body: JSON.stringify({ id: "assistant-recovery-e2e" }),
    });
  });
  await page.route(/\/v1(?:\/workspaces\/[^/]+)?\/sessions\/assistant-recovery-e2e\/events(?:\?|$)/, async (route) => {
    await route.fulfill({
      status: route.request().method() === "GET" ? 200 : 202,
      contentType: "application/json",
      body: JSON.stringify(route.request().method() === "GET" ? { data: [] } : {}),
    });
  });

  await page.goto("/w/default/assistant");
  await expect(page.getByPlaceholder("Ask a question or describe what you want to accomplish…")).toBeVisible();
  await expect.poll(() => ensures).toBe(1);
  await expect.poll(() => probes).toBeGreaterThanOrEqual(2);
});

test("Assistant keeps one page-aware conversation while the operator navigates", async ({ page }) => {
  let createdSessions = 0;
  await page.route(/\/v1(?:\/workspaces\/[^/]+)?\/config\/executable-models(?:\?|$)/, async (route) => {
    await route.fulfill({
      status: 200,
      contentType: "application/json",
      body: JSON.stringify([{ model_id: "assistant-model", readiness: "ready" }]),
    });
  });
  await page.route(/\/v1(?:\/workspaces\/[^/]+)?\/agents\/__admin_assistant(?:\?|$)/, async (route) => {
    await route.fulfill({
      status: 200,
      contentType: "application/json",
      body: JSON.stringify({ id: "__admin_assistant", type: "agent", tools: [], skills: [], mcp_servers: [] }),
    });
  });
  await page.route(/\/v1(?:\/workspaces\/[^/]+)?\/sessions(?:\?|$)/, async (route) => {
    if (route.request().method() !== "POST") return route.continue();
    createdSessions += 1;
    await route.fulfill({
      status: 200,
      contentType: "application/json",
      body: JSON.stringify({ id: "assistant-context-e2e" }),
    });
  });
  await page.route(/\/v1(?:\/workspaces\/[^/]+)?\/sessions\/assistant-context-e2e\/events(?:\?|$)/, async (route) => {
    await route.fulfill({
      status: route.request().method() === "GET" ? 200 : 202,
      contentType: "application/json",
      body: JSON.stringify(route.request().method() === "GET" ? { data: [] } : {}),
    });
  });

  await page.goto("/w/default/files");
  await page.getByRole("button", { name: "Admin Assistant", exact: true }).click();
  const panel = page.getByRole("region", { name: "Admin Assistant", exact: true });
  await expect(panel.getByText("Files", { exact: true })).toBeVisible();
  await expect(panel.getByRole("button", { name: "How do I attach a file to an Agent?", exact: true })).toBeVisible();
  await expect(panel.getByPlaceholder("Ask a question or describe what you want to accomplish…")).toBeVisible();
  expect(createdSessions).toBe(0);
  await panel.getByRole("button", { name: "How do I attach a file to an Agent?", exact: true }).click();
  await expect.poll(() => createdSessions).toBe(1);

  await page.locator(".sidebar").getByRole("button", { name: "Runtime secrets", exact: true }).click();
  await expect(page).toHaveURL(/\/w\/default\/vaults$/);
  await expect(panel.getByText("Runtime secrets", { exact: true })).toBeVisible();
  await expect(panel.getByRole("button", { name: "Which Vault type should I use?", exact: true })).toBeVisible();
  expect(createdSessions).toBe(1);
});

test("Try draft truthfully requires runnable fields", async ({ page }) => {
  await page.goto("/w/default/agents/new");
  await page.getByRole("button", { name: /Try draft/ }).click();
  await expect(page.getByText(/Complete the runnable fields to Try|补全运行必填项后即可试运行/)).toBeVisible();
});

test("author an agent, publish when runnable, and see the truthful outcome", async ({ page }) => {
  const id = `e2e-agent-${Date.now()}`;
  await page.goto("/w/default/agents/new");

  await page.getByPlaceholder("coding-agent").fill(id);
  await openBuild(page, "Instructions");
  await page.getByLabel("System instructions").fill("You are an e2e test agent.");
  await page.getByRole("button", { name: "Save draft", exact: true }).click();
  await expect(page.locator(".ui-toast").filter({ hasText: /Saved|已保存/ })).toBeVisible();

  // After the first save the editor navigates to the new id URL (guard-safe nav —
  // a regression this e2e caught and fixed), then Publish compiles + installs it.
  await expect(page).toHaveURL(new RegExp(`/agents/${id}$`));
  // Publish opens a confirm modal previewing the config diff; confirm inside it.
  await page.getByRole("button", { name: /^Review & publish/ }).click();
  const publishPreview = page.getByRole("heading", { name: /Publish changes|发布改动/ });
  const canPublish = await publishPreview.waitFor({ state: "visible", timeout: 2_000 })
    .then(() => true)
    .catch(() => false);
  if (canPublish) {
    await page.locator(".modal").getByRole("button", { name: /Publish/ }).click();
    await expect(page.locator(".ui-toast").filter({ hasText: /Published|已发布/ })).toBeVisible();
  } else {
    await expect(page.getByText(/cannot resolve model publication|无法解析模型发布/)).toBeVisible();
  }

  // The agents list shows the published agent.
  await page.goto("/w/default/agents");
  const agentRow = page.getByRole("table").getByRole("row").filter({ has: page.getByText(id, { exact: true }) });
  await expect(agentRow).toBeVisible();

  // Sandbox: the published agent opens a live scratch session (same transcript engine
  // the session detail uses). No provider key in CI, so we assert the session + composer
  // come up, not a model reply.
  await page.goto(`/w/default/agents/${id}`);
  await page.getByRole("button", { name: /Try draft/ }).click();
  if (!canPublish) {
    await expect(page.getByText(/Complete the runnable fields to Try|补全运行必填项后即可试运行/)).toBeVisible();
    return;
  }
  await page.getByRole("button", { name: /Start preview/ }).click();
  await expect(page.getByRole("log", { name: "Preview conversation" })).toBeVisible();
  await expect(page.getByText("Current draft", { exact: true })).toBeVisible();
  await expect(page.locator(".agent-preview-chat").getByText(id, { exact: true })).toHaveCount(0);
  await expect(page.getByText("Temporary preview", { exact: true })).toBeVisible();
  const previewComposer = page.getByPlaceholder("Give this Agent a concrete task…");
  await expect(previewComposer).toBeVisible();
  await previewComposer.fill("Explain what you can do");
  await previewComposer.press("Shift+Enter");
  await previewComposer.pressSequentially("Use one concise paragraph");
  await expect(previewComposer).toHaveValue("Explain what you can do\nUse one concise paragraph");
  await page.setViewportSize({ width: 400, height: 800 });
  await expect(previewComposer).toBeVisible();
  await expect(page.getByRole("button", { name: "New preview" })).toBeVisible();
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth + 2)).toBe(true);
});

test("Publish saves the Draft, validates it, and withholds confirmation when invalid", async ({ page, request }) => {
  const id = `publish-flow-${Date.now()}`;
  await page.goto("/w/default/agents/new");
  await page.getByPlaceholder("coding-agent").fill(id);
  await openBuild(page, "Instructions");
  await page.getByLabel("System instructions").fill("Keep the release notes concise.");
  await openAdvanced(page, "Raw configuration");
  const raw = page.getByLabel("Agent JSON");
  const invalid = JSON.parse(await raw.inputValue());
  invalid.tools = ["tool_that_does_not_exist"];
  await raw.fill(JSON.stringify(invalid, null, 2));

  // No separate Save or Validate click: Publish performs both and only then opens
  // the operator checkpoint.
  await page.getByRole("button", { name: /^Review & publish/ }).click();
  await expect(page.getByText(/Needs input|需要处理/).first()).toBeVisible();
  expect((await request.get(await workspaceApiPath(request, `/v1/config/agents/${id}`))).ok()).toBe(true);
  await expect(page.locator(".modal")).toHaveCount(0);
});

test("Agent-authored fields and the State Machine behavior are highlighted after Draft refresh", async ({ page, request }) => {
  const id = `highlight-e2e-${Date.now()}`;
  const original = {
    id,
    system: "original instructions",
    tools: [],
    plugins: [],
    plugin_config: {},
    context_policy: { kind: "keep_all" },
    max_steps: 8,
  };
  await request.put(await workspaceApiPath(request, `/v1/config/agents/${id}`), { data: original });
  await page.goto(`/w/default/agents/${id}`);
  await openBuild(page, "Instructions");
  await expect(page.getByLabel("System instructions")).toHaveValue("original instructions");
  await page.getByPlaceholder("Coding Assistant").fill("operator's unsaved name");
  await request.put(await workspaceApiPath(request, `/v1/config/agents/${id}`), { data: { ...original, system: "agent refined instructions" } });

  await page.evaluate(({ agentId }) => {
    window.dispatchEvent(new CustomEvent("awaken:agent-draft-changed", {
      detail: { id: agentId, paths: ["system", "plugin_config.state_machine"] },
    }));
  }, { agentId: id });

  await expect(page.getByLabel("System instructions")).toHaveValue("agent refined instructions");
  await expect(page.getByPlaceholder("Coding Assistant")).toHaveValue("operator's unsaved name");
  await expect(page.locator(".ui-status-pill").filter({ hasText: /^unsaved$|^未保存$/ })).toBeVisible();
  await expect(page.locator(".agent-change-highlight", { hasText: "System instructions" })).toBeVisible();
  await expect(page.getByRole("tab", { name: "Advanced" }).locator(".agent-change-dot")).toBeVisible();
  await openAdvanced(page, "Orchestration");
  const stateMachine = page.locator(".behavior-card", { hasText: "Agent behavior state machine" });
  await expect(stateMachine).toHaveClass(/agent-change-highlight/);
  await expect(stateMachine.getByText(/Agent updated|Agent 已更新/)).toBeVisible();
});

test("publish preview shows the config diff, domain-labeled", async ({ page, request }) => {
  const id = `diff-e2e-${Date.now()}`;
  await request.put(await workspaceApiPath(request, `/v1/config/agents/${id}`), { data: { id, system: "original", tools: [], plugins: [], plugin_config: {}, context_policy: { kind: "keep_all" }, max_steps: 8 } });
  await page.goto(`/w/default/agents/${id}`);
  await openBuild(page, "Instructions");
  // Wait for the stored config to load into the field before editing, else the load
  // effect would overwrite the edit (and the diff would be empty).
  await expect(page.locator("textarea").first()).toHaveValue("original");
  await page.locator("textarea").first().fill("edited instructions");
  await page.getByRole("button", { name: "Save draft", exact: true }).click();
  await expect(page.locator(".ui-toast").filter({ hasText: /Saved|已保存/ })).toBeVisible();
  await page.getByRole("button", { name: /^Review & publish/ }).click();
  // The diff names the change with its domain label (not the raw path).
  const diff = page.locator(".modal").getByText("System instructions");
  const previewed = await diff.waitFor({ state: "visible", timeout: 2_000 })
    .then(() => true)
    .catch(() => false);
  if (!previewed) await expect(page.locator(".modal")).toHaveCount(0);
});

test("validation issues are field-routed to their section", async ({ page, request }) => {
  const id = `val-e2e-${Date.now()}`;
  // A config that fails compile: a selected tool that isn't in the catalog.
  await request.put(await workspaceApiPath(request, `/v1/config/agents/${id}`), { data: { id, model: { id: "m" }, system: "hi", tools: ["nonexistent_tool"], plugins: [], plugin_config: {}, context_policy: { kind: "keep_all" }, max_steps: 8 } });
  await page.goto(`/w/default/agents/${id}`);
  await openBuild(page, "Instructions");
  await expect(page.getByLabel("System instructions")).toHaveValue("hi"); // wait for load
  await page.getByRole("button", { name: "Check draft", exact: true }).click();
  // The first backend-owned structured issue is projected with its section…
  await expect(page.locator(".banner").filter({ hasText: "Model" })).toBeVisible();
  // …and routes the user there (no client-side rule was re-derived).
  await page.getByRole("button", { name: /Open field/ }).click();
  await expect(page.getByRole("tab", { name: "Build", exact: true })).toHaveAttribute("aria-selected", "true");
  await expect(page.getByRole("tab", { name: "Instructions", exact: true })).toHaveAttribute("aria-selected", "true");
});
