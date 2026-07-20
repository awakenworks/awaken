// Connect Vertex Gemini through the real Console: catalog supply plus a gcloud
// OAuth helper reference. No API key or long-lived Google grant enters Awaken.

import { LIVE_MODEL_ID as MODEL } from "../support/models.mjs";

const PROJECT = process.env.GEMINI_PROJECT ?? "";
const LOCATION = process.env.GEMINI_LOCATION ?? "global";

export const story = {
  promise: "Connect Vertex Gemini through gcloud without copying a long-lived credential into Awaken and prove it can answer immediately.",
  effect: "A Vertex catalog route and allowlisted OAuth helper produce a visible live-model response.",
  aha: "The model answers through a short-lived gcloud token while the long-lived Google grant never enters Awaken.",
  loyalty: "Visible model supply and delegated OAuth build confidence that repeated model operations stay governable.",
  satisfaction: "The story ends with a real answer, removing uncertainty about whether the Vertex setup works.",
  advocacy: "A live Gemini response with no copied API key creates a compact security-and-speed proof to share.",
};

export async function run({ page, goto, say, clearCaption, intro, checkpoint, runtimeCheckpoint, aha, expect, click, type, wait }) {
  if (!PROJECT) throw new Error("01-connect-model requires GEMINI_PROJECT and a working gcloud login");
  const host = LOCATION === "global" ? "aiplatform.googleapis.com" : `${LOCATION}-aiplatform.googleapis.com`;
  const baseUrl = `https://${host}/v1/projects/${PROJECT}/locations/${LOCATION}/`;
  await goto("/w/default/models");
  await intro(
    "Use Gemini without copying a long-lived cloud credential into the platform.",
    "Declare the Vertex route, then bind an allowlisted gcloud OAuth helper that refreshes short-lived tokens.",
  );

  const authorCard = page.locator(".card").filter({ hasText: "Author provider" });
  const inputs = authorCard.locator("input.input");
  await say("Declare Google Vertex AI, its project-scoped endpoint, and the Gemini offering.", 3600);
  await type(inputs.nth(0), "google-vertex");
  await type(inputs.nth(1), "vertex-gemini");
  await type(inputs.nth(2), baseUrl);
  await authorCard.locator("select").selectOption("vertex_gemini");
  await type(page.getByPlaceholder("model-id"), MODEL);
  await type(page.getByPlaceholder("200000"), "1048576");
  await click(authorCard.getByRole("button", { name: /Author|写入/ }));
  await checkpoint("the Vertex Gemini offering is visible in the catalog", async () => {
    await expect(page.getByText(MODEL).first()).toBeVisible();
  });
  await wait(500);

  await goto("/w/default/credentials");
  await say("Choose OAuth and gcloud. Awaken stores a helper id—not an access token or refresh token.", 4200);
  await click(page.getByRole("button", { name: /Enter credential|录入凭证/ }));
  const modal = page.locator(".modal");
  await click(modal.getByRole("button", { name: "oauth", exact: true }));
  await type(modal.locator('input.input').first(), "google-vertex");
  await click(modal.getByRole("button", { name: /Seal & save|密封保存/ }));
  await checkpoint("the gcloud OAuth source is active and secret-free", async () => {
    await expect(modal).toBeHidden();
    const response = await page.request.get("http://127.0.0.1:38080/v1/config/credentials?workspace_id=wrkspc_default");
    const credentials = await response.json();
    const credential = credentials.find((source) => source.provider_id === "google-vertex" && source.kind === "oauth");
    expect(credential).toMatchObject({ status: "active", oauth_helper: "gcloud" });
    expect(credential).not.toHaveProperty("material_ref");
    expect(credential).not.toHaveProperty("oauth_command");
  });
  await wait(500);

  await goto("/w/default/models");
  const row = page.locator("tr", { hasText: MODEL });
  await say("Now test the exact catalog model. The runtime refreshes OAuth and calls Vertex for real.", 3800);
  await click(row.getByRole("button", { name: /Test|测试/ }));
  const composer = page.getByPlaceholder(/Say hello|打个招呼/);
  await type(composer, "Reply with exactly: MODEL READY", { delay: 14 });
  await composer.press("Enter");
  const agentReply = page.locator(".card").filter({ hasText: "⬡ agent" }).last();
  await runtimeCheckpoint("Vertex Gemini returns a visible response through gcloud OAuth", async () => {
    await expect(agentReply).toContainText("MODEL READY", { timeout: 60_000 });
  });
  await wait(900);
  await aha(story.aha);
  await clearCaption();
}
