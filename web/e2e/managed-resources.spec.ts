import { expect, test } from "@playwright/test";

// Locks the console↔endpoint round-trip for the managed-resource surfaces
// (memory / skills / environments / deployments): drive the real UI against the
// real management backend and prove the create/read reaches the endpoint.

test("Memory store: create in the UI and see it listed", async ({ page }) => {
  const name = `mem-${Date.now()}`;
  await page.goto("/w/default/memory");
  await page.getByRole("button", { name: /New memory store/ }).click();
  await page.getByPlaceholder("project-memory").fill(name);
  await page.getByRole("button", { name: "Create", exact: true }).click();
  await expect(page.getByText(name)).toBeVisible();
});

test("Environment: create in the UI and see it listed", async ({ page }) => {
  const name = `env-${Date.now()}`;
  await page.goto("/w/default/environments");
  await page.getByRole("button", { name: /New environment/ }).click();
  await page.getByPlaceholder("my-dev-env").fill(name);
  await page.getByRole("button", { name: "Create", exact: true }).click();
  await expect(page.getByText(name)).toBeVisible();
});

test("Skills: the surface reads the delivered-skill catalog", async ({ page }) => {
  await page.goto("/w/default/skills");
  // Skills come from a durable skill store (SKILL.md), not the console — with none
  // wired the list is empty but live, proving the surface↔endpoint read works.
  await expect(page.getByText(/No skills delivered yet|尚无已交付技能/)).toBeVisible();
});

test("Agent Resources: bind a memory store to an agent and persist it", async ({ page, request }) => {
  const store = `store-${Date.now()}`;
  const agent = `res-agent-${Date.now()}`;
  await request.post("/v1/memory_stores", { data: { name: store } });
  await request.put(`/v1/config/agents/${agent}`, { data: { id: agent, system: "hi", tools: [], plugins: [], plugin_config: {}, context_policy: { kind: "keep_all" }, max_steps: 8 } });

  await page.goto(`/w/default/agents/${agent}`);
  await page.getByRole("button", { name: "Resources", exact: true }).click();
  await page.getByRole("button", { name: /bind a store/ }).click();
  await page.locator("select").first().selectOption({ label: store });
  // The row has one text input (mount path) among two selects (store, access).
  const path = `/mnt/${store}`;
  await page.locator("input").first().fill(path);
  await page.getByRole("button", { name: /Save resources/ }).click();
  await expect(page.locator(".toast").filter({ hasText: /Resources saved|资源已保存/ })).toBeVisible();

  // Reload → the binding rehydrates from the stored resource config.
  await page.reload();
  await page.getByRole("button", { name: "Resources", exact: true }).click();
  await expect(page.locator("input").first()).toHaveValue(path);
});

test("Deployment: create in the UI (agent + environment) and see it listed", async ({ page, request }) => {
  const name = `dep-${Date.now()}`;
  // A deployment needs a published agent + an environment — seed both via the API.
  const agent = `dep-agent-${Date.now()}`;
  await request.put(`/v1/config/agents/${agent}`, { data: { id: agent, name: agent, system: "hi", tools: [], plugins: [], plugin_config: {}, context_policy: { kind: "keep_all" }, max_steps: 8 } });
  await request.post(`/v1/config/agents/${agent}/publish`);
  await request.post("/v1/environments", { data: { name: `dep-env-${Date.now()}`, config: { type: "cloud", networking: { type: "unrestricted" } } } });

  await page.goto("/w/default/deployments");
  await page.getByRole("button", { name: /New deployment/ }).click();
  await page.getByPlaceholder("nightly-report").fill(name);
  await page.locator("select").nth(0).selectOption({ index: 1 }); // agent
  await page.locator("select").nth(1).selectOption({ index: 1 }); // environment
  await page.getByPlaceholder("0 20 * * 5").fill("0 20 * * 5");
  await page.getByRole("button", { name: "Create", exact: true }).click();
  await expect(page.getByText(name)).toBeVisible();
});
