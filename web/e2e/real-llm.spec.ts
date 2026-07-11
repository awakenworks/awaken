import { expect, test } from "@playwright/test";

// Real-LLM e2e via the CONFIG PLANE (no env model config). The backend runs in plain
// management mode — NO AWAKEN_MODEL_SOURCE / GEMINI_API_KEY on the server. The model
// key enters the platform the way an operator would: submitted through the credential
// API (POST /v1/config/credentials) as a vault secret. The runtime then resolves the
// session's model → offering → workspace credential → a real Gemini executor.
//
// Run against a management backend, with the key available to the TEST (not the server):
//   GOOGLE_API_KEY=… pnpm exec playwright test real-llm.spec.ts
// Not in the default CI suite (no key there).

const KEY = process.env.GOOGLE_API_KEY ?? process.env.GEMINI_API_KEY ?? "";
const REPLY = "⬡ agent";
const REPLY_TIMEOUT = 45_000;

test.skip(!KEY, "needs a Gemini/Google key to submit via the credential API");

test("Sandbox answers for real via config-plane credential (no env)", async ({ page, request }) => {
  const id = `real-agent-${Date.now()}`;

  // Operator-style setup, all through the config-plane API — no server env:
  //   provider + endpoint + offering (the model menu) …
  await request.put("/v1/config/providers/google", { data: { id: "google", slug: "google", display_name: "Google", version: 1 } });
  await request.put("/v1/config/endpoints/gemini-ep", { data: { id: "gemini-ep", provider_id: "google", flavor: "gemini", base_url: null, timeout_secs: 60, display_name: "Gemini", version: 1 } });
  await request.post("/v1/config/offerings", { data: { model_id: "gemini-2.5-flash", provider_id: "google", protocol_endpoint_id: "gemini-ep", flavor: "gemini", upstream_model: null } });
  //   … and the KEY as a workspace vault credential (the secret enters via API).
  await request.post("/v1/config/credentials", { data: { workspace_id: "wrkspc_default", kind: "vault", provider_id: "google", secret: KEY } });

  // Author + publish an agent bound to that model, via the console.
  await page.goto("/w/default/agents/new");
  await page.getByPlaceholder("coding-agent").fill(id);
  await page.locator("select").first().selectOption("gemini-2.5-flash");
  await page.locator("textarea").first().fill("You are a terse assistant. Answer in one short sentence.");
  await page.getByRole("button", { name: "Save", exact: true }).click();
  await expect(page.locator(".toast").filter({ hasText: /Saved|已保存/ })).toBeVisible();
  await expect(page).toHaveURL(new RegExp(`/agents/${id}$`));
  await page.getByRole("button", { name: /Publish/ }).click();
  await expect(page.locator(".toast").filter({ hasText: /Published|已发布/ })).toBeVisible();

  // Sandbox: the runtime resolves the model to a REAL Gemini executor from the
  // configured credential (no env), and a real reply lands on screen.
  await page.getByRole("button", { name: "Sandbox", exact: true }).click();
  await page.getByRole("button", { name: /Start session/ }).click();
  const ask = page.getByPlaceholder("Ask the agent…");
  await expect(ask).toBeVisible();
  await ask.fill("Say hello.");
  await ask.press("Enter");
  await expect(page.getByText(REPLY).first()).toBeVisible({ timeout: REPLY_TIMEOUT });
});
