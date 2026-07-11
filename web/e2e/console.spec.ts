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

test("workspace switcher opens the roster dropdown with an add-workspace input", async ({ page }) => {
  await page.goto("/w/default/sessions");
  await page.locator(".ws-crumb").click();
  // Dropdown-specific chrome (the crumb also reads "Default workspace", so scope
  // the assertion to the dropdown's unique caption + add input).
  await expect(page.getByText("Switch workspace")).toBeVisible();
  await expect(page.getByPlaceholder("ws_acme")).toBeVisible();
});

test("agent editor Tools/Plugins are data-driven from /v1/capabilities", async ({ page }) => {
  await page.goto("/w/default/agents/new");
  // Tools tab → CheckPicker fed by capabilities.tools (a hand tool like bash/write).
  await page.getByRole("button", { name: "Tools", exact: true }).click();
  const picker = page.locator(".check-picker").first();
  await expect(picker).toBeVisible();
  await expect(picker.locator(".check-row").first()).toBeVisible();

  // Plugins tab → the schema-carrying plugins from capabilities.plugins.
  await page.getByRole("button", { name: "Plugins & policy" }).click();
  for (const id of ["state_machine", "compact", "memory"]) {
    await expect(page.locator(".check-row", { hasText: id })).toBeVisible();
  }
});

test("Plugins tab renders the Permission policy editor (data-driven from capabilities.policies)", async ({ page }) => {
  await page.goto("/w/default/agents/new");
  await page.getByRole("button", { name: "Plugins & policy" }).click();
  await expect(page.getByText("Permission policy", { exact: true })).toBeVisible();
  await expect(page.getByText("Default decision", { exact: true })).toBeVisible();
  // Add a rule → an editable glob-pattern row appears.
  await page.getByRole("button", { name: /add rule/ }).click();
  await expect(page.getByPlaceholder("Bash(*rm*)")).toBeVisible();
});

test("PermissionEditor authors a rule and persists it through save + reload", async ({ page }) => {
  const id = `perm-e2e-${Date.now()}`;
  await page.goto("/w/default/agents/new");
  await page.getByPlaceholder("coding-agent").fill(id);
  await page.locator("textarea").first().fill("You gate your tools.");
  await page.getByRole("button", { name: "Plugins & policy" }).click();

  const editor = page.locator(".permission-editor");
  // Default decision → Deny (only the default-decision Segmented exists yet).
  await editor.getByRole("button", { name: "Deny" }).first().click();
  await page.getByRole("button", { name: /add rule/ }).click();
  await editor.getByPlaceholder("Bash(*rm*)").fill("Bash(*rm*)");

  await page.getByRole("button", { name: "Save", exact: true }).click();
  await expect(page.locator(".toast").filter({ hasText: /Saved|已保存/ })).toBeVisible();
  await expect(page).toHaveURL(new RegExp(`/agents/${id}$`));

  // Reload → the authored policy rehydrates from the stored config (round-trips).
  await page.reload();
  await page.getByRole("button", { name: "Plugins & policy" }).click();
  await expect(editor.getByPlaceholder("Bash(*rm*)")).toHaveValue("Bash(*rm*)");
});

test("enabling a plugin renders a schema-driven form (not raw JSON)", async ({ page }) => {
  await page.goto("/w/default/agents/new");
  await page.getByRole("button", { name: "Plugins & policy" }).click();
  // Enable `compact` → its config_schema (numeric knobs) renders as a form.
  await page.locator(".check-row", { hasText: "compact" }).getByRole("checkbox").check();
  // The per-plugin section is schema-driven and exposes compact's fields.
  await expect(page.getByText("schema-driven")).toBeVisible();
  await expect(page.getByText("keep_last")).toBeVisible();
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

test("Admin Assistant is truth-driven: gates when the assistant agent isn't installed", async ({ page }) => {
  await page.goto("/w/default/assistant");
  await expect(page.getByRole("heading", { name: /Admin Assistant|控制台助手/ })).toBeVisible();
  // __admin_assistant is not installed in CI → the note explains why, not a fake copilot.
  await expect(page.getByText(/Admin Assistant agent is not installed|助手 agent 尚未安装/)).toBeVisible();
});

test("Sandbox tab gates an unpublished draft (nothing live to talk to yet)", async ({ page }) => {
  await page.goto("/w/default/agents/new");
  await page.getByRole("button", { name: "Sandbox", exact: true }).click();
  await expect(page.getByText(/Publish to test in the Sandbox|发布后即可在 Sandbox 试运行/)).toBeVisible();
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
  await page.getByRole("button", { name: /Publish/ }).click();
  await expect(page.locator(".toast").filter({ hasText: /Published|已发布/ })).toBeVisible();

  // The agents list shows the published agent.
  await page.goto("/w/default/agents");
  await expect(page.getByText(id)).toBeVisible();

  // Sandbox: a published agent is installed, so a live scratch session opens here
  // (the same transcript engine the session detail uses). No provider key in CI, so
  // we assert the session + composer come up, not a model reply.
  await page.goto(`/w/default/agents/${id}`);
  await page.getByRole("button", { name: "Sandbox", exact: true }).click();
  await page.getByRole("button", { name: /Start session/ }).click();
  await expect(page.getByPlaceholder("Ask the agent…")).toBeVisible();
});
