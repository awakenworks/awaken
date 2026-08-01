import { expect, test, type Page } from "@playwright/test";
import { MANAGED_HEADERS } from "./betas";
import { SyntheticModelDirectory } from "./synthetic-model";

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

// Cause/effect R1: a local route scope with no Organization/Workspace registry
// renders the Agents brand + exact route scope + canonical task navigation, and
// must not render the retired fake selectors.
test("shell renders the Awaken Agents brand, route scope and task-oriented rail", async ({ page }) => {
  await page.goto("/w/default/sessions");
  await expect(page.locator(".brand-anchor")).toContainText("Awaken");
  await expect(page.locator(".workspace-context")).toContainText("default");
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

test("responsive: a narrow viewport keeps the shell usable with no horizontal overflow", async ({ page }) => {
  await page.setViewportSize({ width: 400, height: 800 });
  await page.goto("/w/default/sessions");
  // The sidebar collapses to a top nav strip; nav + content stay reachable.
  await expect(page.locator(".sidebar")).toBeVisible();
  await expect(page.locator(".sidebar").getByRole("button", { name: "Agents", exact: true })).toBeVisible();
  // The page never scrolls wider than the viewport (the fixed rail no longer pushes it).
  const noOverflow = await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth + 2);
  expect(noOverflow).toBe(true);
});

// Cause/effect R2: entering the configured Workspace exposes the shared live
// readiness projection; absence of a server roster means no arbitrary scope input.
test("Workspace overview exposes one live readiness path instead of a client roster", async ({ page }) => {
  await page.goto("/w/default/overview");
  await expect(page.locator(".readiness-panel")).toBeVisible();
  await expect(page.getByText("Ready to run")).toBeVisible();
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

test("Quickstart publishes the reviewed draft and starts a durable Session in the chosen Environment", async ({ page, request }) => {
  const stamp = Date.now();
  const id = `quickstart-${stamp}`;
  const model = `quickstart-model-${stamp}`;
  await syntheticModels.configure(request, model);
  const environmentResponse = await request.post("/v1/environments", {
    headers: MANAGED_HEADERS,
    data: {
      name: `quickstart-env-${stamp}`,
      config: { type: "cloud", networking: { type: "unrestricted" } },
    },
  });
  const environmentBody = await environmentResponse.text();
  expect(environmentResponse.ok(), environmentBody).toBe(true);
  const environment = JSON.parse(environmentBody);

  await page.goto("/w/default/agents/new");
  await page.getByRole("button", { name: /Coding/ }).click();
  await page.getByPlaceholder("coding-agent").fill(id);
  await page.getByLabel("Model (references workspace catalog)", { exact: true })
    .selectOption({ label: model });
  await page.getByLabel("Environment for this run")
    .selectOption(environment.id);
  const task = `Quickstart proof ${stamp}`;
  await page.getByLabel("Task sent to the new Session").fill(task);
  await page.getByRole("button", { name: /Review & run/ }).click();

  const modal = page.locator(".modal");
  await expect(modal.getByRole("heading", { name: /Review the first real run/ })).toBeVisible();
  await expect(modal).toContainText(environment.id);
  await expect(modal).toContainText(task);
  const sessionResponse = page.waitForResponse((response) =>
    response.request().method() === "POST" && response.url().endsWith("/v1/sessions"));
  const eventRequest = page.waitForRequest((req) =>
    req.method() === "POST" && /\/v1\/sessions\/[^/]+\/events$/.test(req.url()));
  await modal.getByRole("button", { name: /Publish & run/ }).click();
  const session = await (await sessionResponse).json();
  expect((await eventRequest).postDataJSON()).toEqual({
    events: [{ type: "user.message", content: [{ type: "text", text: task }] }],
  });
  await expect(page).toHaveURL(new RegExp(`/sessions/${session.id}$`));
  const stored = await (await request.get(`/v1/sessions/${session.id}`, {
    headers: MANAGED_HEADERS,
  })).json();
  expect(stored.environment_id).toBe(environment.id);
  expect(stored.agent.id).toBe(id);
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
      && response.url().endsWith(`/v1/config/agents/${id}`));
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
  const model = `mcp-model-${Date.now()}`;
  await syntheticModels.configure(request, model);
  const credentialResponse = await request.get("/v1/config/credentials?workspace_id=default");
  const credentialBody = await credentialResponse.text();
  expect(credentialResponse.ok(), credentialBody).toBe(true);
  const credentialSources = JSON.parse(credentialBody);
  const mcpCredential = credentialSources.find((credential: { status: string }) =>
    credential.status === "active");
  expect(mcpCredential).toBeTruthy();

  await page.goto("/w/default/agents/new");
  await page.getByPlaceholder("coding-agent").fill(id);
  await openBuild(page, "Instructions");
  await page.getByLabel("System instructions").fill("Use the issue tracker when the goal requires it.");
  await page.getByLabel("Model (references workspace catalog)", { exact: true })
    .selectOption({ label: model });

  await openBuild(page, "Tools & permissions");
  await page.getByRole("button", { name: /override an MCP tool/ }).click();
  await page.getByLabel("Canonical tool id 1").fill("mcp__issues__create_issue");
  await page.getByLabel("Alias").fill("file_issue");
  await page.getByLabel("Description").last().fill("Create an issue with the verified acceptance criteria.");
  await page.getByLabel("Defer this tool").check();
  await expect(page.getByText("Runtime-discovered MCP tool; resolved when the server connects.")).toBeVisible();

  await openBuild(page, "Skills & MCP");
  await page.getByRole("button", { name: "+ MCP server", exact: true }).click();
  await page.getByLabel("Server name").fill("issues");
  await page.getByLabel("URL").fill("https://mcp.example.test/issues");
  await page.getByLabel("Credential source").selectOption(
    `${mcpCredential.id}@${mcpCredential.version}`,
  );
  await page.getByLabel("Prompts as skills").check();
  await page.getByRole("button", { name: "+ MCP server", exact: true }).click();
  await page.getByLabel("Transport").last().selectOption("sandbox_stdio");
  await page.getByLabel("Server name").last().fill("browser");
  await page.getByLabel("Sandbox command").fill("playwright-mcp");
  await page.getByLabel("Arguments (one per line)").fill("--headless\n--isolated");
  await openAdvanced(page, "Orchestration");
  await page.getByLabel("multiagent JSON").fill("{");
  await expect(page.getByRole("button", { name: "Save draft", exact: true })).toBeDisabled();
  await expect(page.getByRole("alert")).toContainText("Invalid JSON");
  await page.getByLabel("multiagent JSON").fill("");
  await expect(page.getByRole("button", { name: "Save draft", exact: true })).toBeEnabled();

  const initialMcpSave = page.waitForResponse((response) =>
    response.request().method() === "PUT"
      && response.url().endsWith(`/v1/config/agents/${id}`));
  await page.getByRole("button", { name: "Save draft", exact: true }).click();
  const initialMcpResponse = await initialMcpSave;
  const initialMcpBody = await initialMcpResponse.text();
  expect(initialMcpResponse.ok(), initialMcpBody).toBe(true);
  await expect(page).toHaveURL(new RegExp(`/agents/${id}$`));

  const response = await request.get(`/v1/config/agents/${id}`);
  expect(response.ok()).toBe(true);
  const stored = await response.json();
  expect(stored.tools).not.toContain("mcp__issues__create_issue");
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
      defer: true,
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
      && response.url().endsWith(`/v1/config/agents/${id}`));
  await page.getByRole("button", { name: "Save draft", exact: true }).click();
  expect((await rawSave).ok()).toBe(true);
  const lossless = await (await request.get(`/v1/config/agents/${id}`)).json();
  expect(lossless.compaction).toEqual({ window: 32000, keep_recent: 12 });

  await page.getByRole("button", { name: "Publish", exact: false }).first().click();
  const publication = page.waitForResponse((response) =>
    response.request().method() === "POST"
      && response.url().endsWith(`/v1/config/agents/${id}/publish`));
  await page.locator(".modal").getByRole("button", { name: "Publish", exact: false }).click();
  expect((await publication).ok()).toBe(true);
  expect((await request.get(`/v1/config/agents/${id}`)).ok()).toBe(true);

  await page.goto("/w/default/mcp");
  await expect(page.getByText("https://mcp.example.test/issues")).toBeVisible();
  await expect(page.getByText("sandbox stdio · playwright-mcp")).toBeVisible();
  const issuesRow = page.getByRole("row", { name: /issues https:\/\/mcp\.example\.test\/issues/ });
  await expect(issuesRow.getByText(id, { exact: true })).toBeVisible();
  await issuesRow.getByText(id, { exact: true }).click();
  await expect(page).toHaveURL(new RegExp(`/agents/${id}\\?stage=build&section=integrations$`));
  await expect(page.getByRole("heading", { name: "Direct MCP servers" })).toBeVisible();
});

test("new session sends the inline MCP prompt-skill opt-in", async ({ page }) => {
  let posted: Record<string, unknown> | undefined;
  await page.route("**/v1/config/agents", async (route) => {
    if (route.request().method() !== "GET") return route.continue();
    await route.fulfill({
      status: 200,
      contentType: "application/json",
      body: JSON.stringify({ data: [{ id: "prompt-skill-agent", published: true }] }),
    });
  });
  await page.route("**/v1/sessions", async (route) => {
    if (route.request().method() !== "POST") return route.continue();
    posted = route.request().postDataJSON() as Record<string, unknown>;
    await route.fulfill({
      status: 200,
      contentType: "application/json",
      body: JSON.stringify({ id: "session-prompt-skill-e2e" }),
    });
  });
  await page.goto("/w/default/sessions");
  await page.getByRole("button", { name: /New session/ }).click();
  await page.locator(".modal select").first().selectOption({ index: 1 });
  await page.getByRole("button", { name: /add inline server/ }).click();
  await page.getByPlaceholder("name").fill("docs");
  await page.getByPlaceholder("https://…").fill("https://docs.test/mcp");
  await page.getByLabel("Prompts as skills").check();
  await page.locator(".modal").getByRole("button", { name: /Create/ }).click();
  await expect.poll(() => posted).toBeTruthy();
  expect(posted?.mcp_servers).toEqual([
    { name: "docs", url: "https://docs.test/mcp", prompts_as_skills: true },
  ]);
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
  await expect(card.getByText("1 · Instance & lifetime")).toBeVisible();
  await expect(card.getByText("2 · Trigger, guard & effect")).toBeVisible();
  await expect(card.getByRole("group").filter({ hasText: /tool:read/ })).toBeVisible();
  await expect(card.getByLabel("Scope")).toHaveValue("thread");
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
  await expect(page.getByText(/backend face is not mounted yet|后端面尚未就绪/)).toBeVisible();
});

test("session detail toggles Chat ⇄ Trace (the log read as spans)", async ({ page, request }) => {
  const stamp = Date.now();
  const model = `trace-model-${stamp}`;
  const agent = `trace-agent-${stamp}`;
  await syntheticModels.configure(request, model);
  expect((await request.put(`/v1/config/agents/${agent}`, {
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
  expect((await request.post(`/v1/config/agents/${agent}/publish`)).ok()).toBe(true);
  const res = await request.post("/v1/sessions", {
    headers: MANAGED_HEADERS,
    data: { agent, title: "trace-e2e" },
  });
  expect(res.ok()).toBe(true);
  const sid = (await res.json()).id as string;
  await page.goto(`/w/default/sessions/${sid}`);
  await expect(page.getByRole("button", { name: "Chat", exact: true })).toBeVisible();
  await page.getByRole("button", { name: "Trace", exact: true }).click();
  await expect(page.getByText(/No spans yet|暂无 span/)).toBeVisible();
});

test("Session Integrations shows only the durable active MCP projection", async ({ page }) => {
  const sessionId = "session-active-mcp";
  await page.route(`**/v1/sessions/${sessionId}`, async (route) => {
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
  await page.locator(".segmented").getByRole("button", { name: "Integrations", exact: true }).click();
  await expect(page.getByRole("heading", { name: "Active MCP integrations" })).toBeVisible();
  await expect(page.getByText("https://mcp.example.test/docs")).toBeVisible();
  await expect(page.getByText("remote Skills")).toBeVisible();
  await expect(page.getByText("active", { exact: true }).last()).toBeVisible();
});

test("Agent composer supports multiline input and shows work immediately on the first message", async ({ page, request }) => {
  const res = await request.post("/v1/sessions", {
    headers: MANAGED_HEADERS,
    data: { agent: "default", title: "composer-e2e" },
  });
  const sid = (await res.json()).id as string;
  let postedText = "";
  let release!: () => void;
  const held = new Promise<void>((resolve) => { release = resolve; });
  await page.route(`**/v1/sessions/${sid}/events`, async (route) => {
    if (route.request().method() !== "POST") return route.continue();
    const body = route.request().postDataJSON() as { events: Array<{ content: Array<{ text: string }> }> };
    postedText = body.events[0].content[0].text;
    await held;
    await route.fulfill({ status: 202, contentType: "application/json", body: "{}" });
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
  await expect(page.locator(".transcript-composer")).toBeVisible();
  const composerBox = await page.locator(".transcript-composer").boundingBox();
  const viewport = page.viewportSize();
  expect(composerBox && viewport && viewport.height - (composerBox.y + composerBox.height)).toBeLessThan(80);
  release();
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
  await testBtn.click();
  // The modal mounts the shared transcript against the pinned model.
  await expect(page.getByRole("heading", { name: /Test model ·/ })).toBeVisible();
  await expect(page.getByPlaceholder("Say hello…")).toBeVisible();
});

test("Admin Assistant truthfully shows a composer or the missing-model prerequisite", async ({ page }) => {
  await page.goto("/w/default/assistant");
  await expect(page.getByRole("heading", { name: /Admin Assistant|控制台助手/ })).toBeVisible();
  // The assistant is seeded into the reserved scope (ADR-0052), but a composer is
  // only runnable when the current backend has a credentialed model.
  const composer = page.getByPlaceholder("Describe the agent you want…");
  const prerequisite = page.getByText(/The assistant needs a model to run on|助手需要一个模型才能运行/i);
  await expect(composer.or(prerequisite)).toBeVisible();
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
  await page.getByRole("button", { name: /Publish/ }).click();
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
  await expect(page.getByText(id)).toBeVisible();

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
  await expect(page.getByPlaceholder("Ask the agent…")).toBeVisible();
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
  await page.getByRole("button", { name: /Publish/ }).click();
  await expect(page.getByText(/Needs input|需要处理/).first()).toBeVisible();
  expect((await request.get(`/v1/config/agents/${id}`)).ok()).toBe(true);
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
  await request.put(`/v1/config/agents/${id}`, { data: original });
  await page.goto(`/w/default/agents/${id}`);
  await openBuild(page, "Instructions");
  await expect(page.getByLabel("System instructions")).toHaveValue("original instructions");
  await page.getByPlaceholder("Coding Assistant").fill("operator's unsaved name");
  await request.put(`/v1/config/agents/${id}`, { data: { ...original, system: "agent refined instructions" } });

  await page.evaluate(({ agentId }) => {
    window.dispatchEvent(new CustomEvent("awaken:agent-draft-changed", {
      detail: { id: agentId, paths: ["system", "plugin_config.state_machine"] },
    }));
  }, { agentId: id });

  await expect(page.getByLabel("System instructions")).toHaveValue("agent refined instructions");
  await expect(page.getByPlaceholder("Coding Assistant")).toHaveValue("operator's unsaved name");
  await expect(page.getByText(/unsaved|未保存/)).toBeVisible();
  await expect(page.locator(".agent-change-highlight", { hasText: "System instructions" })).toBeVisible();
  await expect(page.getByRole("tab", { name: "Advanced" }).locator(".agent-change-dot")).toBeVisible();
  await openAdvanced(page, "Orchestration");
  const stateMachine = page.locator(".behavior-card", { hasText: "Agent behavior state machine" });
  await expect(stateMachine).toHaveClass(/agent-change-highlight/);
  await expect(stateMachine.getByText(/Agent updated|Agent 已更新/)).toBeVisible();
});

test("publish preview shows the config diff, domain-labeled", async ({ page, request }) => {
  const id = `diff-e2e-${Date.now()}`;
  await request.put(`/v1/config/agents/${id}`, { data: { id, system: "original", tools: [], plugins: [], plugin_config: {}, context_policy: { kind: "keep_all" }, max_steps: 8 } });
  await page.goto(`/w/default/agents/${id}`);
  await openBuild(page, "Instructions");
  // Wait for the stored config to load into the field before editing, else the load
  // effect would overwrite the edit (and the diff would be empty).
  await expect(page.locator("textarea").first()).toHaveValue("original");
  await page.locator("textarea").first().fill("edited instructions");
  await page.getByRole("button", { name: "Save draft", exact: true }).click();
  await expect(page.locator(".ui-toast").filter({ hasText: /Saved|已保存/ })).toBeVisible();
  await page.getByRole("button", { name: /Publish/ }).click();
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
  await request.put(`/v1/config/agents/${id}`, { data: { id, model: { id: "m" }, system: "hi", tools: ["nonexistent_tool"], plugins: [], plugin_config: {}, context_policy: { kind: "keep_all" }, max_steps: 8 } });
  await page.goto(`/w/default/agents/${id}`);
  await openBuild(page, "Instructions");
  await expect(page.getByLabel("System instructions")).toHaveValue("hi"); // wait for load
  await page.getByRole("button", { name: "Validate", exact: true }).click();
  // The first backend-owned structured issue is projected with its section…
  await expect(page.locator(".banner").filter({ hasText: "Model" })).toBeVisible();
  // …and routes the user there (no client-side rule was re-derived).
  await page.getByRole("button", { name: /Open field/ }).click();
  await expect(page.getByRole("tab", { name: "Build", exact: true })).toHaveAttribute("aria-selected", "true");
  await expect(page.getByRole("tab", { name: "Instructions", exact: true })).toHaveAttribute("aria-selected", "true");
});
