// Connect Vertex Gemini through the consolidated Provider Connection workflow:
// one descriptor-driven form verifies gcloud OAuth, persists the credential and
// catalog once, then explicitly binds the Workspace profile.

import { LIVE_MODEL_ID as MODEL } from "../support/models.mjs";

const PROJECT = process.env.GEMINI_PROJECT ?? "";
const LOCATION = process.env.GEMINI_LOCATION ?? "global";

export const story = {
  promise: "Connect Vertex Gemini once, without copying credentials between model and vault screens, and prove the saved route can answer.",
  effect: "One Provider Connection command verifies gcloud OAuth and makes imported models immediately available to Agents.",
  aha: "One guided connection replaces duplicate provider, credential, and model setup—and the live model answers through short-lived OAuth.",
  loyalty: "One authoritative connection status and explicit routing policy make repeated model operations predictable.",
  satisfaction: "The story ends with a real answer after one continuous setup, removing uncertainty and duplicate entry.",
  advocacy: "A live Gemini response from a no-copy OAuth workflow creates a compact security-and-speed proof to share.",
};

export async function run({ page, goto, say, clearCaption, intro, checkpoint, runtimeCheckpoint, aha, expect, click, type, wait }) {
  if (!PROJECT) throw new Error("01-connect-model requires GEMINI_PROJECT and a working gcloud login");
  await goto("/w/default/models");
  await intro(
    "Connect Gemini without repeating provider, credential, and model configuration across separate screens.",
    "Awaken renders one backend-described Provider Connection, verifies gcloud OAuth, imports models, and hands the exact route to the Workspace profile.",
  );

  const connectionCard = page.locator(".card").filter({ hasText: /Provider connections|供应商连接/ });
  await say("Choose Vertex AI. Its project, location, protocol, and OAuth method come from the installed backend descriptor.", 4200);
  await click(connectionCard.getByRole("button", { name: /Vertex AI/ }));
  await type(connectionCard.getByLabel("Google Cloud project"), PROJECT);
  await type(connectionCard.getByLabel("Location"), LOCATION);
  await say("Verify once. Awaken mints a short-lived gcloud token, imports the live model directory, and persists one reusable credential source.", 4600);
  await click(connectionCard.getByRole("button", { name: /Verify & import models|验证并导入模型/ }));
  await checkpoint("one Provider Connection is Ready with a secret-free gcloud source", async () => {
    await expect(connectionCard.getByText(/Credential verified and models imported|凭证已验证，模型已导入/)).toBeVisible();
    const response = await page.request.get("http://127.0.0.1:38080/v1/config/credentials?workspace_id=wrkspc_default");
    const credentials = await response.json();
    const credential = credentials.find((source) => source.provider_id === "vertex" && source.kind === "oauth");
    expect(credential).toMatchObject({ status: "active", oauth_helper: "gcloud" });
    expect(credential).not.toHaveProperty("material_ref");
    expect(credential).not.toHaveProperty("oauth_command");
    const summaries = await (await page.request.get("http://127.0.0.1:38080/v1/config/provider-connections?workspace_id=wrkspc_default")).json();
    expect(summaries.find((summary) => summary.provider_id === "vertex")).toMatchObject({ status: "ready" });
  });
  await wait(500);

  await say("The verified connection is immediately available to Auto agents—there is no second Workspace-default step.", 3800);
  await checkpoint("the connected model is immediately discoverable for Agent authoring", async () => {
    const catalog = await (await page.request.get("http://127.0.0.1:38080/v1/config/catalog")).json();
    expect(catalog.offerings.some((offering) => offering.model_id === MODEL && offering.provider_id === "vertex")).toBe(true);
  });
  await wait(500);

  const row = page.locator("tr", { hasText: MODEL });
  await say("Now send a real prompt through the saved route. Runtime refreshes OAuth and calls the exact Vertex offering.", 3800);
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
