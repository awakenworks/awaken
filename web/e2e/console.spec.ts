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
  for (const title of ["Auto-compaction", "Memory recall", "Tool-call ordering"]) {
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
  await expect(page.getByPlaceholder("Bash(*rm*)")).toBeVisible();
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
  await editor.getByPlaceholder("Bash(*rm*)").fill("Bash(*rm*)");

  await page.getByRole("button", { name: "Save", exact: true }).click();
  await expect(page.locator(".toast").filter({ hasText: /Saved|已保存/ })).toBeVisible();
  await expect(page).toHaveURL(new RegExp(`/agents/${id}$`));

  // Reload → the authored policy rehydrates from the stored config (round-trips).
  await page.reload();
  await page.getByRole("tab", { name: "Tools" }).click();
  await expect(editor.getByPlaceholder("Bash(*rm*)")).toHaveValue("Bash(*rm*)");
});

test("enabling a behavior renders a schema-driven form (not raw JSON)", async ({ page }) => {
  await page.goto("/w/default/agents/new");
  await page.getByRole("tab", { name: "Behavior" }).click();
  // Toggle the Auto-compaction behavior on → its config_schema renders as a form.
  await page.locator(".behavior-card", { hasText: "Auto-compaction" }).getByRole("switch").check();
  // The schema-driven form exposes compact's fields (e.g. keep_last).
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

test("Admin Assistant is live: the seeded assistant opens a session composer", async ({ page }) => {
  await page.goto("/w/default/assistant");
  await expect(page.getByRole("heading", { name: /Admin Assistant|控制台助手/ })).toBeVisible();
  // The assistant is seeded into the reserved scope (ADR-0052), so the surface opens
  // a live session composer rather than the "not installed" gate.
  await expect(page.getByPlaceholder("Describe the agent you want…")).toBeVisible();
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
  await page.getByRole("button", { name: /Publish/ }).click();
  await expect(page.locator(".toast").filter({ hasText: /Published|已发布/ })).toBeVisible();

  // The agents list shows the published agent.
  await page.goto("/w/default/agents");
  await expect(page.getByText(id)).toBeVisible();

  // Sandbox: a published agent is installed, so a live scratch session opens here
  // (the same transcript engine the session detail uses). No provider key in CI, so
  // we assert the session + composer come up, not a model reply.
  await page.goto(`/w/default/agents/${id}`);
  await page.getByRole("button", { name: /Try it/ }).click();
  await page.getByRole("button", { name: /Start session/ }).click();
  await expect(page.getByPlaceholder("Ask the agent…")).toBeVisible();
});
