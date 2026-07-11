import { expect, test } from "@playwright/test";

// Real-LLM e2e: drives the console against a backend booted with a live model
// (AWAKEN_MODEL_SOURCE=gemini + GEMINI_API_KEY). Unlike console.spec.ts (which runs
// against the in-process model and asserts only that plumbing comes up), these tests
// assert a REAL model reply lands on screen. Run with a Gemini backend already up:
//   AWAKEN_MODEL_MODE=management AWAKEN_MODEL_SOURCE=gemini GEMINI_API_KEY=… cargo run -p awaken-server-local
//   pnpm exec playwright test real-llm.spec.ts
// Not part of the default CI suite (no key there).

// A real reply is an agent message card (the "⬡ agent" marker) with non-empty text.
const REPLY = "⬡ agent";
const REPLY_TIMEOUT = 45_000;

test("Admin Assistant answers for real (live Gemini)", async ({ page }) => {
  await page.goto("/w/default/assistant");
  const composer = page.getByPlaceholder("Describe the agent you want…");
  await expect(composer).toBeVisible();
  await composer.fill("In one short sentence, what can you help me do?");
  await composer.press("Enter");
  // The seeded assistant runs on the live model → a real reply card appears.
  await expect(page.getByText(REPLY).first()).toBeVisible({ timeout: REPLY_TIMEOUT });
});

test("Sandbox answers for real: publish an agent, then talk to it (live Gemini)", async ({ page, request }) => {
  const id = `real-agent-${Date.now()}`;
  // Seed a catalog offering for the live model so the editor's model picker lists it
  // and the published agent routes to Gemini (genai keys the adapter off the model id).
  await request.put("/v1/config/providers/google", { data: { id: "google", slug: "google", display_name: "Google", version: 1 } });
  await request.put("/v1/config/endpoints/gemini-ep", { data: { id: "gemini-ep", provider_id: "google", flavor: "gemini", base_url: null, timeout_secs: 60, display_name: "Gemini", version: 1 } });
  await request.post("/v1/config/offerings", { data: { model_id: "gemini-2.5-flash", provider_id: "google", protocol_endpoint_id: "gemini-ep", flavor: "gemini", upstream_model: null } });

  await page.goto("/w/default/agents/new");
  await page.getByPlaceholder("coding-agent").fill(id);
  await page.locator("select").first().selectOption("gemini-2.5-flash");
  await page.locator("textarea").first().fill("You are a terse assistant. Answer in one short sentence.");
  await page.getByRole("button", { name: "Save", exact: true }).click();
  await expect(page.locator(".toast").filter({ hasText: /Saved|已保存/ })).toBeVisible();
  await expect(page).toHaveURL(new RegExp(`/agents/${id}$`));
  await page.getByRole("button", { name: /Publish/ }).click();
  await expect(page.locator(".toast").filter({ hasText: /Published|已发布/ })).toBeVisible();

  // Sandbox: open a live scratch session against the published agent and ask it.
  await page.getByRole("button", { name: "Sandbox", exact: true }).click();
  await page.getByRole("button", { name: /Start session/ }).click();
  const ask = page.getByPlaceholder("Ask the agent…");
  await expect(ask).toBeVisible();
  await ask.fill("Say hello.");
  await ask.press("Enter");
  await expect(page.getByText(REPLY).first()).toBeVisible({ timeout: REPLY_TIMEOUT });
});
