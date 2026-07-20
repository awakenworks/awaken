import { expect, test } from "@playwright/test";

// Drives the real console against a real management backend. Covers the shell/nav,
// the capability-driven agent editor (S1–S3), truth-driven gating (S4), the config
// agent author→publish lifecycle, and the workspace switcher.

test("shell renders the topbar org + workspace switcher and the scoped rail", async ({ page }) => {
  await page.goto("/w/default/sessions");
  // Topbar: org anchor + workspace switcher (design handoff moved these here).
  await expect(page.locator(".org-anchor")).toContainText("Awaken");
  await expect(page.locator(".ws-crumb")).toBeVisible();
  // Rail: data-driven nav items.
  const rail = page.locator(".sidebar");
  await expect(rail.getByRole("button", { name: "Sessions" })).toBeVisible();
  await expect(rail.getByRole("button", { name: "Agents", exact: true })).toBeVisible();
  await expect(rail.getByRole("button", { name: "Models" })).toBeVisible();
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

test("workspace switcher opens the roster dropdown with an add-workspace input", async ({ page }) => {
  await page.goto("/w/default/sessions");
  await page.locator(".ws-crumb").click();
  // Dropdown-specific chrome (the crumb also reads "Default workspace", so scope
  // the assertion to the dropdown's unique caption + add input).
  await expect(page.getByText("Switch workspace")).toBeVisible();
  await expect(page.getByPlaceholder("ws_acme")).toBeVisible();
});

test("agent editor Tools/Behavior are data-driven from /v1/capabilities", async ({ page }) => {
  await page.goto("/w/default/agents/new");
  // Tools tab → CheckPicker fed by capabilities.tools (a hand tool like bash/write).
  await page.getByRole("tab", { name: "Tools" }).click();
  const picker = page.locator(".check-picker").first();
  await expect(picker).toBeVisible();
  await expect(picker.locator(".check-row").first()).toBeVisible();

  // Behavior tab → the schema-carrying plugins from capabilities.plugins, rendered as
  // named behavior cards (not raw ids).
  await page.getByRole("tab", { name: "Behavior" }).click();
  for (const title of ["Auto-compaction", "Memory recall", "Agent behavior state machine"]) {
    await expect(page.locator(".behavior-card", { hasText: title })).toBeVisible();
  }
});

test("Tools tab renders the Permissions editor (data-driven from capabilities.policies)", async ({ page }) => {
  await page.goto("/w/default/agents/new");
  await page.getByRole("tab", { name: "Tools" }).click();
  await expect(page.getByText("Permissions", { exact: true })).toBeVisible();
  await expect(page.getByText("Default decision", { exact: true })).toBeVisible();
  // Add a rule → an editable glob-pattern row appears.
  await page.getByRole("button", { name: /add rule/ }).click();
  await expect(page.getByPlaceholder('bash(command ~ "*rm -rf*")')).toBeVisible();
});

test("PermissionEditor authors a rule and persists it through save + reload", async ({ page }) => {
  const id = `perm-e2e-${Date.now()}`;
  await page.goto("/w/default/agents/new");
  await page.getByPlaceholder("coding-agent").fill(id);
  await page.locator("textarea").first().fill("You gate your tools.");
  await page.getByRole("tab", { name: "Tools" }).click();

  const editor = page.locator(".permission-editor");
  // Default decision → Deny (only the default-decision Segmented exists yet).
  await editor.getByRole("button", { name: "Deny" }).first().click();
  await page.getByRole("button", { name: /add rule/ }).click();
  const pattern = 'bash(command ~ "*rm -rf*")';
  await editor.getByPlaceholder(pattern).fill(pattern);

  await page.getByRole("button", { name: "Save", exact: true }).click();
  await expect(page.locator(".toast").filter({ hasText: /Saved|已保存/ })).toBeVisible();
  await expect(page).toHaveURL(new RegExp(`/agents/${id}$`));

  // Reload → the authored policy rehydrates from the stored config (round-trips).
  await page.reload();
  await page.getByRole("tab", { name: "Tools" }).click();
  await expect(editor.getByPlaceholder(pattern)).toHaveValue(pattern);
});

test("Agent editor persists and publishes a direct MCP binding plus MCP tool override", async ({ page, request }) => {
  const id = `mcp-agent-${Date.now()}`;
  await page.goto("/w/default/agents/new");
  await page.getByPlaceholder("coding-agent").fill(id);
  await page.getByLabel("System instructions").fill("Use the issue tracker when the goal requires it.");

  await page.getByRole("tab", { name: "Tools", exact: true }).click();
  await page.getByRole("button", { name: /override an MCP tool/ }).click();
  await page.getByLabel("Canonical tool id 1").fill("mcp__issues__create_issue");
  await page.getByLabel("Alias").fill("file_issue");
  await page.getByLabel("Description").last().fill("Create an issue with the verified acceptance criteria.");
  await page.getByLabel("Defer this tool").check();
  await expect(page.getByText(/Runtime-discovered MCP tool/)).toBeVisible();

  await page.getByRole("tab", { name: "Integrations", exact: true }).click();
  await page.getByRole("button", { name: "+ MCP server", exact: true }).click();
  await page.getByLabel("Server name").fill("issues");
  await page.getByLabel("URL").fill("https://mcp.example.test/issues");
  await page.getByRole("button", { name: "+ Skill", exact: true }).click();
  await page.getByLabel("Skill id").fill("issue-writing");
  await page.getByLabel("multiagent JSON").fill("{");
  await expect(page.getByRole("button", { name: "Save", exact: true })).toBeDisabled();
  await expect(page.getByRole("alert")).toContainText("Invalid JSON");
  await page.getByLabel("multiagent JSON").fill('{"strategy":"managed"}');
  await expect(page.getByRole("button", { name: "Save", exact: true })).toBeEnabled();

  await page.getByRole("button", { name: "Save", exact: true }).click();
  await expect(page.locator(".toast").filter({ hasText: /Saved|已保存/ })).toBeVisible();
  await expect(page).toHaveURL(new RegExp(`/agents/${id}$`));

  const response = await request.get(`/v1/config/agents/${id}`);
  expect(response.ok()).toBe(true);
  const stored = await response.json();
  expect(stored.tools).not.toContain("mcp__issues__create_issue");
  expect(stored.mcp_servers).toEqual([
    { type: "url", name: "issues", url: "https://mcp.example.test/issues" },
  ]);
  expect(stored.skills).toEqual([{ id: "issue-writing" }]);
  expect(stored.multiagent).toEqual({ strategy: "managed" });
  expect(stored.tool_overrides).toEqual([
    {
      target: "mcp__issues__create_issue",
      alias: "file_issue",
      description: "Create an issue with the verified acceptance criteria.",
      defer: true,
    },
  ]);

  await page.reload();
  await page.getByRole("button", { name: "{} JSON" }).click();
  const rawEditor = page.getByLabel("Agent JSON");
  await expect(rawEditor).toHaveValue(/mcp__issues__create_issue/);
  const rawConfig = JSON.parse(await rawEditor.inputValue());
  rawConfig.compaction = { window: 32000, keep_recent: 12 };
  await rawEditor.fill(JSON.stringify(rawConfig, null, 2));
  await page.getByRole("button", { name: "Save", exact: true }).click();
  await expect(page.locator(".toast").filter({ hasText: /Saved|已保存/ })).toBeVisible();
  const lossless = await (await request.get(`/v1/config/agents/${id}`)).json();
  expect(lossless.compaction).toEqual({ window: 32000, keep_recent: 12 });

  await page.getByRole("button", { name: "Publish", exact: false }).first().click();
  await page.locator(".modal").getByRole("button", { name: "Publish", exact: false }).click();
  await expect(page.locator(".toast").filter({ hasText: /Published|已发布/ })).toBeVisible();
});

test("enabling a behavior renders a schema-driven form (not raw JSON)", async ({ page }) => {
  await page.goto("/w/default/agents/new");
  await page.getByRole("tab", { name: "Behavior" }).click();
  // Toggle the Auto-compaction behavior on → its config_schema renders as a form.
  await page.locator(".behavior-card", { hasText: "Auto-compaction" }).getByRole("switch").check();
  // The schema-driven form exposes compact's fields (e.g. keep_last).
  await expect(page.getByText("keep_last")).toBeVisible();
});

test("State Machine editor exposes scope, pre-execution gate, lifecycle events and request context", async ({ page }) => {
  await page.goto("/w/default/agents/new");
  await page.getByRole("tab", { name: "Behavior" }).click();
  const card = page.locator(".behavior-card", { hasText: "Agent behavior state machine" });
  await card.getByRole("switch").check();

  await card.getByRole("button", { name: /Read before write/ }).click();
  await expect(card.getByText("1 · Instance & lifetime")).toBeVisible();
  await expect(card.getByText("2 · Trigger, guard & effect")).toBeVisible();
  await expect(card.getByText("pre + post").first()).toBeVisible();
  await expect(card.getByLabel("Scope")).toHaveValue("thread");
  await expect(card.getByLabel("Before run action").last()).toHaveValue("deny");

  await card.getByRole("button", { name: /Todo reminder/ }).click();
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
  // A fresh session (via the vite proxy) has no events → the Trace view shows its
  // empty-spans hint, proving the toggle switched away from the chat composer.
  const res = await request.post("/v1/sessions", { data: { agent: "default", title: "trace-e2e" } });
  const sid = (await res.json()).id as string;
  await page.goto(`/w/default/sessions/${sid}`);
  await expect(page.getByRole("button", { name: "Chat", exact: true })).toBeVisible();
  await page.getByRole("button", { name: "Trace", exact: true }).click();
  await expect(page.getByText(/No spans yet|暂无 span/)).toBeVisible();
});

test("Agent composer supports multiline input and shows work immediately on the first message", async ({ page, request }) => {
  const res = await request.post("/v1/sessions", { data: { agent: "default", title: "composer-e2e" } });
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

test("Models Test opens a live model dialog (scratch session + composer)", async ({ page }) => {
  // The smoke seeds an offering; if the catalog is empty, author one via the UI first.
  await page.goto("/w/default/models");
  const testBtn = page.getByRole("button", { name: "Test", exact: true }).first();
  if ((await testBtn.count()) === 0) {
    await page.getByRole("button", { name: "Author", exact: true }).click();
    await page.waitForTimeout(300);
  }
  await page.getByRole("button", { name: "Test", exact: true }).first().click();
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

test("Sandbox tab gates an unpublished draft (nothing live to talk to yet)", async ({ page }) => {
  await page.goto("/w/default/agents/new");
  await page.getByRole("button", { name: /Try it/ }).click();
  await expect(page.getByText(/Publish to test in the Sandbox|发布后即可在 Sandbox 试运行/)).toBeVisible();
});

test("model picker offers only credentialed models", async ({ page, request }) => {
  const model = `acme-${Date.now()}`;
  // A provider + offering with NO credential yet.
  await request.put("/v1/config/providers/acme", { data: { id: "acme", slug: "acme", display_name: "Acme", version: 1 } });
  await request.put("/v1/config/endpoints/acme-ep", { data: { id: "acme-ep", provider_id: "acme", dialect: "open_ai_chat", base_url: "https://acme.example/v1/", timeout_secs: 60, display_name: "Acme", version: 1 } });
  await request.post("/v1/config/offerings", { data: { model_id: model, provider_id: "acme", protocol_endpoint_id: "acme-ep", dialect: "open_ai_chat", upstream_model: null } });

  await page.goto("/w/default/agents/new");
  // Uncredentialed → not selectable (hidden from the picker).
  await expect(page.locator("option", { hasText: model })).toHaveCount(0);

  // Add a credential for its provider → the model becomes selectable.
  await request.post("/v1/config/credentials", { data: { workspace_id: "wrkspc_default", kind: "vault", provider_id: "acme", secret: "sk-acme-test" } }); // awaken-allow: secret (synthetic e2e fixture)
  await page.reload();
  await expect(page.locator("option", { hasText: model })).toHaveCount(1);
});

test("author → publish a config agent, and see it in the list", async ({ page }) => {
  const id = `e2e-agent-${Date.now()}`;
  await page.goto("/w/default/agents/new");

  await page.getByPlaceholder("coding-agent").fill(id);
  // System instructions (a textarea in Basics).
  await page.locator("textarea").first().fill("You are an e2e test agent.");
  await page.getByRole("button", { name: "Save", exact: true }).click();
  await expect(page.locator(".toast").filter({ hasText: /Saved|已保存/ })).toBeVisible();

  // After the first save the editor navigates to the new id URL (guard-safe nav —
  // a regression this e2e caught and fixed), then Publish compiles + installs it.
  await expect(page).toHaveURL(new RegExp(`/agents/${id}$`));
  // Publish opens a confirm modal previewing the config diff; confirm inside it.
  await page.getByRole("button", { name: /Publish/ }).click();
  await expect(page.getByRole("heading", { name: /Publish changes|发布改动/ })).toBeVisible();
  await page.locator(".modal").getByRole("button", { name: /Publish/ }).click();
  await expect(page.locator(".toast").filter({ hasText: /Published|已发布/ })).toBeVisible();

  // The agents list shows the published agent.
  await page.goto("/w/default/agents");
  await expect(page.getByText(id)).toBeVisible();

  // Sandbox: the published agent opens a live scratch session (same transcript engine
  // the session detail uses). No provider key in CI, so we assert the session + composer
  // come up, not a model reply.
  await page.goto(`/w/default/agents/${id}`);
  await page.getByRole("button", { name: /Try it/ }).click();
  await page.getByRole("button", { name: /Start session/ }).click();
  await expect(page.getByPlaceholder("Ask the agent…")).toBeVisible();
});

test("publish preview shows the config diff, domain-labeled", async ({ page, request }) => {
  const id = `diff-e2e-${Date.now()}`;
  await request.put(`/v1/config/agents/${id}`, { data: { id, system: "original", tools: [], plugins: [], plugin_config: {}, context_policy: { kind: "keep_all" }, max_steps: 8 } });
  await page.goto(`/w/default/agents/${id}`);
  // Wait for the stored config to load into the field before editing, else the load
  // effect would overwrite the edit (and the diff would be empty).
  await expect(page.locator("textarea").first()).toHaveValue("original");
  await page.locator("textarea").first().fill("edited instructions");
  await page.getByRole("button", { name: "Save", exact: true }).click();
  await expect(page.locator(".toast").filter({ hasText: /Saved|已保存/ })).toBeVisible();
  await page.getByRole("button", { name: /Publish/ }).click();
  // The diff names the change with its domain label (not the raw path).
  await expect(page.locator(".modal").getByText("System instructions")).toBeVisible();
});

test("validation issues are field-routed to their section", async ({ page, request }) => {
  const id = `val-e2e-${Date.now()}`;
  // A config that fails compile: a selected tool that isn't in the catalog.
  await request.put(`/v1/config/agents/${id}`, { data: { id, model: { id: "m" }, system: "hi", tools: ["nonexistent_tool"], plugins: [], plugin_config: {}, context_policy: { kind: "keep_all" }, max_steps: 8 } });
  await page.goto(`/w/default/agents/${id}`);
  await expect(page.locator("textarea").first()).toHaveValue("hi"); // wait for load
  await page.getByRole("button", { name: "Validate", exact: true }).click();
  // The backend's structured issue is projected to a banner labeled for its section…
  await expect(page.locator(".banner").filter({ hasText: "Tools" })).toBeVisible();
  // …and routes the user there (no client-side rule was re-derived).
  await page.getByRole("button", { name: /Go to section/ }).click();
  await expect(page.getByRole("tab", { name: "Tools" })).toHaveAttribute("aria-selected", "true");
});
