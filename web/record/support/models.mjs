const BACKEND = "http://127.0.0.1:38080";

export const LIVE_MODEL_ID = process.env.GEMINI_MODEL ?? "gemini-2.5-flash";
export const LIVE_MODEL_LABEL = "Vertex Gemini";

/** Configure the real model through the same catalog and credential APIs the UI
 * uses. The long-lived Google grant stays in gcloud; Awaken stores only the
 * allowlisted helper id and materializes a short-lived OAuth token per run. */
export async function configureLiveModel(page) {
  const project = process.env.GEMINI_PROJECT ?? "";
  const location = process.env.GEMINI_LOCATION ?? "global";
  if (!project) throw new Error("this runtime-effect story requires GEMINI_PROJECT");
  const provider = "google-vertex";
  const endpoint = "vertex-gemini";
  const host = location === "global" ? "aiplatform.googleapis.com" : `${location}-aiplatform.googleapis.com`;
  const baseUrl = `https://${host}/v1/projects/${project}/locations/${location}/`;

  await page.request.put(`${BACKEND}/v1/config/providers/${provider}`, {
    data: { id: provider, slug: provider, display_name: "Google Vertex AI", version: 1 },
  });
  await page.request.put(`${BACKEND}/v1/config/endpoints/${endpoint}`, {
    data: { id: endpoint, provider_id: provider, dialect: "vertex_gemini", base_url: baseUrl, timeout_secs: 60, display_name: "Vertex Gemini", version: 1 },
  });
  await page.request.post(`${BACKEND}/v1/config/offerings`, {
    data: { model_id: LIVE_MODEL_ID, provider_id: provider, protocol_endpoint_id: endpoint, dialect: "vertex_gemini", upstream_model: null },
  });
  const credentials = await (await page.request.get(`${BACKEND}/v1/config/credentials?workspace_id=wrkspc_default`)).json();
  if (!credentials.some((source) => source.provider_id === provider && source.kind === "oauth" && source.status === "active")) {
    const entered = await page.request.post(`${BACKEND}/v1/config/credentials`, {
      data: { workspace_id: "wrkspc_default", kind: "oauth", provider_id: provider, oauth_helper: "gcloud" },
    });
    if (!entered.ok()) throw new Error(`could not register gcloud OAuth credential: ${entered.status()} ${await entered.text()}`);
  }
}

export async function configureSyntheticModel(page, id) {
  const provider = `${id}-provider`;
  const endpoint = `${id}-endpoint`;
  await page.request.put(`${BACKEND}/v1/config/providers/${provider}`, {
    data: { id: provider, slug: provider, display_name: "Recording fixture", version: 1 },
  });
  await page.request.put(`${BACKEND}/v1/config/endpoints/${endpoint}`, {
    data: { id: endpoint, provider_id: provider, dialect: "anthropic_messages", base_url: "https://recording.invalid/v1", timeout_secs: 60, display_name: "Recording fixture", version: 1 },
  });
  await page.request.post(`${BACKEND}/v1/config/offerings`, {
    data: { model_id: id, provider_id: provider, protocol_endpoint_id: endpoint, dialect: "anthropic_messages", upstream_model: id },
  });
  await page.request.post(`${BACKEND}/v1/config/credentials`, {
    data: { workspace_id: "wrkspc_default", kind: "vault", provider_id: provider, secret: "recording-fixture-only" }, // awaken-allow: secret (synthetic fixture)
  });
}
