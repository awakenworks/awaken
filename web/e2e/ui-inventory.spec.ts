import { expect, test } from "@playwright/test";
import { NAV, navPath } from "../src/lib/navigation/paths";

const VIEWPORTS = [
  { label: "mobile", width: 390, height: 844 },
  { label: "tablet", width: 768, height: 1024 },
  { label: "desktop", width: 1280, height: 900 },
] as const;

// Decision table: mobile/tablet/desktop × each navigation destination ->
// one heading, no pending skeleton/error, and no horizontal document overflow.
// Modal selectors use public dialog semantics after deleting local recipes.
// User-perspective smoke inventory: every rail destination must render a usable main
// surface, avoid an error state, and remain horizontally usable at each release
// breakpoint. The attached inventory and screenshot make the audit evidence useful
// for information architecture, visual review, and video planning.
for (const item of NAV) {
  for (const viewport of VIEWPORTS) {
    test(`UI inventory · ${viewport.label} · ${item.group} · ${item.label}`, async ({ page }, testInfo) => {
      await page.setViewportSize(viewport);
      await page.goto(navPath(item, "default"));
      const main = page.locator("main");
      await expect(main).toBeVisible();
      await expect(main.locator("h1")).toHaveCount(1);
      await expect(main.locator(".ui-skeleton-bar, .ui-state--loading")).toHaveCount(0);
      await expect(main.locator(".err, .ui-state--danger")).toHaveCount(0);
      expect(await page.evaluate(() => document.documentElement.scrollWidth <= document.documentElement.clientWidth)).toBe(true);

      const inventory = await main.evaluate((root) => ({
        headings: [...root.querySelectorAll("h1,h2,h3")].map((node) => node.textContent?.trim()).filter(Boolean),
        buttons: [...root.querySelectorAll("button")].map((node) => node.textContent?.trim()).filter(Boolean),
        fields: [...root.querySelectorAll("input,textarea,select")].map((node) =>
          node.getAttribute("placeholder") || node.getAttribute("aria-label") || node.tagName.toLowerCase()),
        tabs: [...root.querySelectorAll('[role="tab"]')].map((node) => node.textContent?.trim()).filter(Boolean),
        gates: [...root.querySelectorAll(".gate")].map((node) => node.textContent?.trim()).filter(Boolean),
      }));
      await testInfo.attach(`${item.key}-${viewport.label}-inventory.json`, {
        body: Buffer.from(`${JSON.stringify({ ...item, viewport, inventory }, null, 2)}\n`),
        contentType: "application/json",
      });
      await testInfo.attach(`${item.key}-${viewport.label}.png`, {
        body: await page.screenshot({ fullPage: true }),
        contentType: "image/png",
      });
    });
  }
}

test("Agent configuration exposes every controllable capability by user intent", async ({ page }) => {
  await page.goto("/w/default/agents/new");
  await expect(page.getByRole("button", { name: "Check draft", exact: true })).toBeVisible();
  await expect(page.getByRole("button", { name: "Save draft", exact: true })).toBeVisible();
  await expect(page.getByRole("button", { name: /Review & publish/ })).toBeVisible();
  for (const tab of ["Quickstart", "Build", "Advanced"]) {
    await expect(page.getByRole("tab", { name: tab, exact: true })).toBeVisible();
  }

  await expect(page.getByPlaceholder("coding-agent")).toBeVisible();
  await expect(page.getByLabel("Display name", { exact: true })).toBeVisible();
  await expect(page.getByLabel("Task sent to the new Session")).toBeVisible();
  await expect(page.getByLabel("Environment for this run")).toBeVisible();

  await page.getByRole("tab", { name: "Build", exact: true }).click();
  await page.getByRole("tab", { name: "Instructions", exact: true }).click();
  await expect(page.getByText(/Context window policy/)).toBeVisible();
  await page.getByRole("switch", { name: "Auto-compaction" }).check();
  const compact = page.locator(".behavior-card", { hasText: "Auto-compaction" });
  await expect(compact.getByText("Compaction instructions", { exact: true })).toBeVisible();
  await expect(compact.locator("textarea").first()).toBeVisible();

  await page.getByRole("tab", { name: "Memory & resources", exact: true }).click();
  const memory = page.locator(".behavior-card", { hasText: /Memory/ });
  await memory.getByRole("switch").check();
  await expect(memory.getByText("Memory extraction instructions", { exact: true })).toBeVisible();
  await expect(memory.getByText("Extraction task prompt", { exact: true })).toBeVisible();
  await expect(memory.getByLabel("Memory extraction instructions", { exact: true })).toBeVisible();
  await expect(memory.getByLabel("Extraction task prompt", { exact: true })).toBeVisible();

  await page.getByRole("tab", { name: "Advanced", exact: true }).click();
  await page.getByRole("tab", { name: "Orchestration", exact: true }).click();
  await page.getByRole("switch", { name: "Agent behavior state machine" }).check();
  const machine = page.locator(".behavior-card", { hasText: "Agent behavior state machine" });
  await expect(machine.getByRole("button", { name: /Background-task reminder/ })).toBeVisible();
  await expect(machine.getByRole("button", { name: /Todo reminder/i })).toBeVisible();

  await page.getByRole("tab", { name: "Build", exact: true }).click();
  await page.getByRole("tab", { name: "Tools & permissions", exact: true }).click();
  // Typed ToolSet authoring owns permissions after the legacy editor removal.
  await expect(page.getByText("Permission preset", { exact: true })).toBeVisible();
  // The tool ID is stable; its current catalog description is product copy.
  await expect(page.getByRole("checkbox", { name: /^bash\b/ })).toBeVisible();
  // Advanced policies are collapsed initially; expansion exposes the same
  // canonical tool controls without adding another authoring surface.
  await page.getByText("Advanced tool presentation and background policies", { exact: true }).click();
  await expect(page.getByRole("button", { name: /override an MCP tool/ })).toBeVisible();

  await page.getByRole("tab", { name: "Skills & MCP", exact: true }).click();
  await expect(page.getByRole("heading", { name: "MCP integrations" })).toBeVisible();
  await expect(page.getByRole("link", { name: "MCP connection and ToolSet guide ↗" }))
    .toHaveAttribute("href", "https://awakenworks.com/docs/agents/protocols/mcp/");
  await expect(page.getByRole("heading", { name: "Skill bindings" })).toBeVisible();
  await expect(page.getByText(/state only the goal/)).toBeVisible();

  await page.getByRole("tab", { name: "Advanced", exact: true }).click();
  await page.getByRole("tab", { name: "Raw configuration", exact: true }).click();
  await expect(page.getByLabel("Agent JSON")).toBeVisible();
  await expect(page.getByText(/Lossless Agent object view/)).toBeVisible();

  await page.getByRole("button", { name: /Try draft/ }).click();
  await expect(page.getByText(/Complete the runnable fields to Try/)).toBeVisible();
});

// Cause/effect inventory: each authoritative aggregate exposes exactly one primary
// creation/setup entry and its complete pre-commit form. Provider setup owns model
// connection; inference credentials owns Claude setup-token; no MCP catalog parallel
// path is expected.
test("Primary create flows disclose their required configuration before commit", async ({ page }) => {
  const flows = [
    { path: "/w/default/environments", button: /New environment/, heading: /New environment/ },
    { path: "/w/default/memory", button: /New memory store/, heading: /New memory store/ },
    { path: "/w/default/sessions", button: /New session/, heading: /New session/ },
    { path: "/w/default/deployments", button: /New deployment/, heading: /New deployment/ },
    { path: "/w/default/credentials", button: /Claude Code setup token/, heading: /Add Claude Code setup token/ },
  ];
  for (const flow of flows) {
    await page.goto(flow.path);
    await page.getByRole("button", { name: flow.button }).click();
    const modal = page.getByRole("dialog");
    await expect(modal.getByRole("heading", { name: flow.heading })).toBeVisible();
    await expect(modal.locator("input,textarea,select,button").first()).toBeVisible();
  }

  await page.goto("/w/default/models");
  await expect(page.getByRole("heading", { name: /Provider connections/ })).toBeVisible();

  await page.goto("/w/default/protocols");
  const consoleOrigin = new URL(page.url()).origin;
  const connectionAddresses = page.locator(".connection-address");
  await expect(connectionAddresses.nth(0)).toContainText(consoleOrigin);
  await expect(connectionAddresses.nth(1)).toContainText(`${consoleOrigin}/v1/capabilities`);
  await expect(page.locator(".protocol-endpoint").first()).toContainText(`${consoleOrigin}/v1/sessions`);
  await expect(connectionAddresses.nth(0).getByRole("button", { name: "Copy Base URL" })).toBeVisible();
  for (const protocol of ["Managed Agents", "Vercel AI SDK", "AG-UI", "A2A", "MCP Server"]) {
    await expect(page.getByText(`How to connect ${protocol}`, { exact: true })).toBeVisible();
  }
  await page.goto("/w/default/settings");
  await expect(page.getByRole("heading", { name: "Connections & access" })).toBeVisible();
  const settingsGrid = page.locator(".settings-grid");
  for (const destination of ["API & protocols", "MCP overview", "Webhooks", "A2A federation"]) {
    await expect(settingsGrid.getByRole("button", { name: new RegExp(destination) })).toBeVisible();
  }
  // Topology decision table: embedded IAM mounts Access and its token action;
  // explicit no-login hides the navigation item. A direct link remains stable
  // and explains that the capability is unavailable, so a bookmark or copied
  // URL does not disappear into an unrelated page.
  await page.goto("/w/default/access");
  await expect(page).toHaveURL(/\/w\/default\/access$/);
  await expect(page.locator(".sidebar").getByRole("button", { name: "Access", exact: true })).toHaveCount(0);
  await expect(page.getByRole("heading", { name: "Not available in this deployment" })).toBeVisible();
  await expect(page.getByRole("button", { name: "Return to overview" })).toBeVisible();
  await expect(page.getByRole("button", { name: "Review available configuration" })).toBeVisible();
});

test("Protocol connection addresses stay readable on a narrow viewport", async ({ page }) => {
  await page.setViewportSize({ width: 390, height: 844 });
  await page.goto("/w/default/protocols");

  await expect(page.locator(".connection-address").nth(0).getByRole("button", { name: "Copy Base URL" })).toBeVisible();
  await expect(page.locator(".connection-address").nth(1).getByRole("button", { name: "Copy check URL" })).toBeVisible();
  await expect(page.locator(".protocol-endpoint").first().getByRole("button", { name: "Copy endpoint" })).toBeVisible();
  await expect.poll(() => page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth))
    .toBe(true);
});

test("shared command dialog preserves search, navigation and dismissal", async ({ page }) => {
  // R1 open -> focused input + bounded shared dialog; R2 query -> matching
  // product route; R3 Escape -> closed; R4 selection -> navigation + closed.
  await page.goto("/w/default/agents");
  const trigger = page.getByRole("button", { name: /Search/ }).first();
  await trigger.click();
  const dialog = page.getByRole("dialog", { name: "Command palette" });
  const search = dialog.getByPlaceholder("Search pages and workflows…");
  await expect(search).toBeFocused();
  await search.fill("Sessions");
  await expect(dialog.getByRole("button", { name: /Sessions/ })).toBeVisible();
  await page.keyboard.press("Escape");
  await expect(dialog).toHaveCount(0);
  await trigger.click();
  await search.fill("Sessions");
  await dialog.getByRole("button", { name: /Sessions/ }).click();
  await expect(page).toHaveURL(/\/sessions$/);
  await expect(dialog).toHaveCount(0);
});
