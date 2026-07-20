import { expect, test } from "@playwright/test";
import { NAV, navPath } from "../src/lib/navigation/paths";

// User-perspective smoke inventory: every rail destination must render a usable main
// surface, avoid an error state, and remain horizontally usable at the product's
// desktop recording viewport. The attached inventory makes the audit evidence useful
// for information architecture and video planning instead of reducing it to pass/fail.
for (const item of NAV) {
  test(`UI inventory · ${item.group} · ${item.label}`, async ({ page }, testInfo) => {
    await page.goto(navPath(item, "default"));
    const main = page.locator("main");
    await expect(main).toBeVisible();
    await expect(main.locator(".err")).toHaveCount(0);
    expect(await page.evaluate(() => document.documentElement.scrollWidth <= document.documentElement.clientWidth)).toBe(true);

    const inventory = await main.evaluate((root) => ({
      headings: [...root.querySelectorAll("h1,h2,h3")].map((node) => node.textContent?.trim()).filter(Boolean),
      buttons: [...root.querySelectorAll("button")].map((node) => node.textContent?.trim()).filter(Boolean),
      fields: [...root.querySelectorAll("input,textarea,select")].map((node) =>
        node.getAttribute("placeholder") || node.getAttribute("aria-label") || node.tagName.toLowerCase()),
      tabs: [...root.querySelectorAll('[role="tab"]')].map((node) => node.textContent?.trim()).filter(Boolean),
      gates: [...root.querySelectorAll(".gate")].map((node) => node.textContent?.trim()).filter(Boolean),
    }));
    await testInfo.attach(`${item.key}-inventory.json`, {
      body: Buffer.from(`${JSON.stringify({ ...item, inventory }, null, 2)}\n`),
      contentType: "application/json",
    });
  });
}

test("Agent configuration exposes every controllable capability by user intent", async ({ page }) => {
  await page.goto("/w/default/agents/new");
  for (const tab of ["Overview", "Behavior", "Tools", "Integrations", "Resources"]) {
    await expect(page.getByRole("tab", { name: tab, exact: true })).toBeVisible();
  }

  await expect(page.getByPlaceholder("coding-agent")).toBeVisible();
  await expect(page.locator("textarea").first()).toBeVisible();

  await page.getByRole("tab", { name: "Behavior", exact: true }).click();
  await expect(page.getByText(/Context window policy/)).toBeVisible();
  for (const capability of ["Auto-compaction", "Memory recall", "Agent behavior state machine"]) {
    await expect(page.locator(".behavior-card", { hasText: capability })).toBeVisible();
  }
  await page.getByRole("switch", { name: "Auto-compaction" }).check();
  const compact = page.locator(".behavior-card", { hasText: "Auto-compaction" });
  await expect(compact.getByText("Compaction instructions", { exact: true })).toBeVisible();
  await expect(compact.locator("textarea").first()).toBeVisible();

  await page.getByRole("switch", { name: "Memory recall" }).check();
  const memory = page.locator(".behavior-card", { hasText: "Memory recall" });
  await expect(memory.getByText("Memory extraction instructions", { exact: true })).toBeVisible();
  await expect(memory.getByText("Extraction task prompt", { exact: true })).toBeVisible();
  await expect(memory.getByLabel("Memory extraction instructions", { exact: true })).toBeVisible();
  await expect(memory.getByLabel("Extraction task prompt", { exact: true })).toBeVisible();

  await page.getByRole("switch", { name: "Agent behavior state machine" }).check();
  const machine = page.locator(".behavior-card", { hasText: "Agent behavior state machine" });
  await expect(machine.getByRole("button", { name: /Background-task reminder/ })).toBeVisible();
  await expect(machine.getByRole("button", { name: /Todo reminder/i })).toBeVisible();

  await page.getByRole("tab", { name: "Tools", exact: true }).click();
  await expect(page.getByText("Permissions", { exact: true })).toBeVisible();
  await expect(page.getByRole("checkbox", { name: /bash Run a shell command/ })).toBeVisible();
  await expect(page.getByRole("button", { name: /override an MCP tool/ })).toBeVisible();

  await page.getByRole("tab", { name: "Integrations", exact: true }).click();
  await expect(page.getByRole("heading", { name: "Direct MCP servers" })).toBeVisible();
  await expect(page.getByRole("heading", { name: "Skill optimization" })).toBeVisible();
  await expect(page.getByText(/state only the goal/)).toBeVisible();

  await page.getByRole("button", { name: "{} JSON" }).click();
  await expect(page.getByLabel("Agent JSON")).toBeVisible();
  await expect(page.getByText(/Lossless Agent object view/)).toBeVisible();

  await page.getByRole("tab", { name: "Resources", exact: true }).click();
  await expect(page.getByText(/Save the agent first, then bind resources/)).toBeVisible();

  await page.getByRole("button", { name: /Try it/ }).click();
  await expect(page.getByText(/Publish to test in the Sandbox/)).toBeVisible();
});

test("Primary create flows disclose their required configuration before commit", async ({ page }) => {
  const flows = [
    { path: "/w/default/environments", button: /New environment/, heading: /New environment/ },
    { path: "/w/default/memory", button: /New memory store/, heading: /New memory store/ },
    { path: "/w/default/sessions", button: /New session/, heading: /New session/ },
    { path: "/w/default/deployments", button: /New deployment/, heading: /New deployment/ },
    { path: "/w/default/credentials", button: /Enter credential/, heading: /Enter credential/ },
  ];
  for (const flow of flows) {
    await page.goto(flow.path);
    await page.getByRole("button", { name: flow.button }).click();
    const modal = page.locator(".modal");
    await expect(modal.getByRole("heading", { name: flow.heading })).toBeVisible();
    await expect(modal.locator("input,textarea,select,button").first()).toBeVisible();
  }

  await page.goto("/w/default/models");
  await expect(page.getByRole("heading", { name: /Author provider/ })).toBeVisible();
  await page.goto("/w/default/mcp-servers");
  await expect(page.getByRole("heading", { name: /Author MCP server/ })).toBeVisible();
  await page.goto("/w/default/access");
  const mint = page.getByRole("heading", { name: /Mint token/ });
  const gate = page.getByText(/embedded IAM|嵌入式 IAM/).first();
  await expect(mint.or(gate)).toBeVisible();
});
