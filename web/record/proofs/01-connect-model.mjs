// Connect one real provider through the consolidated Provider Connection
// workflow. The recorder can use Vertex OAuth or a write-only API key without
// changing the product path or duplicating credential/model setup.

import {
  LIVE_MODEL_AUTH as AUTH,
  LIVE_MODEL_ID as MODEL,
  LIVE_MODEL_LABEL as PROVIDER_LABEL,
  LIVE_MODEL_PROVIDER as PROVIDER,
} from "../support/models.mjs";
import { BACKEND, requireJson } from "../support/control-plane.mjs";

const PROJECT = process.env.GEMINI_PROJECT ?? "";
const LOCATION = process.env.GEMINI_LOCATION ?? "global";
const API_KEY = process.env.AWAKEN_RECORD_LIVE_API_KEY
  ?? (PROVIDER === "deepseek" ? process.env.DEEPSEEK_API_KEY : process.env.OPENAI_API_KEY)
  ?? "";

export async function run({ page, goto, checkpoint, runtimeCheckpoint, expect, click, type }) {
  if (PROVIDER === "vertex" && !PROJECT) {
    throw new Error("01-connect-model requires GEMINI_PROJECT and a working gcloud login for Vertex");
  }
  if (PROVIDER !== "vertex" && !API_KEY) {
    throw new Error(`01-connect-model requires a write-only ${PROVIDER_LABEL} API key`);
  }
  const workspace = await requireJson(
    await page.request.get(`${BACKEND}/v1/config/workspace-context`),
    "recording Workspace context",
  );
  const workspaceQuery = encodeURIComponent(workspace.workspace_id);
  await goto("/w/default/models");

  const connectionCard = page.locator(".card").filter({ hasText: /Provider connections|供应商连接/ });
  await click(connectionCard.getByRole("button", { name: new RegExp(PROVIDER_LABEL) }));
  if (PROVIDER === "vertex") {
    await type(connectionCard.getByLabel("Google Cloud project"), PROJECT);
    await type(connectionCard.getByLabel("Location"), LOCATION);
  } else {
    // Selecting a provider intentionally defaults to credential reuse when a
    // prior recording already established one. This chapter demonstrates the
    // new-key path, so choose it explicitly before addressing the secret field.
    await click(connectionCard.getByRole("button", { name: /New API key|新 API Key/, exact: true }));
    await type(connectionCard.getByLabel(/API key \(write-only\)|API Key（仅写入）/), API_KEY, { delay: 1 });
  }
  await click(connectionCard.getByRole("button", { name: /Verify & import models|验证并导入模型/ }));
  await checkpoint(`one Provider Connection is Ready with ${AUTH}`, async () => {
    await expect(connectionCard.getByText(/Credential verified and models imported|凭证已验证，模型已导入/)).toBeVisible();
    const credentials = await requireJson(
      await page.request.get(`${BACKEND}/v1/config/credentials?workspace_id=${workspaceQuery}`),
      `${PROVIDER_LABEL} credential readback`,
    );
    const credential = credentials.find((source) => source.provider_id === PROVIDER);
    expect(credential).toMatchObject({ status: "active" });
    if (PROVIDER === "vertex") expect(credential).toMatchObject({ kind: "oauth", oauth_helper: "gcloud" });
    else expect(credential).toMatchObject({ kind: "vault" });
    expect(credential).not.toHaveProperty("material_ref");
    expect(credential).not.toHaveProperty("oauth_command");
    const summaries = await requireJson(
      await page.request.get(`${BACKEND}/v1/config/provider-connections?workspace_id=${workspaceQuery}`),
      "Provider Connection readback",
    );
    expect(summaries.find((summary) => summary.provider_id === PROVIDER)).toMatchObject({ status: "ready" });
  });

  await checkpoint("the connected model is immediately discoverable for Agent authoring", async () => {
    const catalog = await requireJson(await page.request.get(`${BACKEND}/v1/config/catalog`), "Model catalog readback");
    expect(catalog.offerings.some((offering) => offering.model_id === MODEL && offering.provider_id === PROVIDER)).toBe(true);
  });

  const row = page.getByRole("row").filter({
    has: page.getByRole("cell", { name: MODEL, exact: true }),
  });
  await click(row.getByRole("button", { name: /Test|测试/ }));
  await runtimeCheckpoint(`${PROVIDER_LABEL} returns a visible response through ${AUTH}`, async () => {
    await expect(
      page.locator('.model-test-modal [data-role="assistant"]').getByText("MODEL READY", { exact: true }),
    ).toBeVisible({ timeout: 60_000 });
  });
}
