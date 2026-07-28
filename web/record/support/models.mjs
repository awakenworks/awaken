const BACKEND = "http://127.0.0.1:38080";

export const LIVE_MODEL_ID = process.env.GEMINI_MODEL ?? "gemini-2.5-flash";
export const LIVE_MODEL_LABEL = "Vertex Gemini";

/** Configure the real model through the same single Provider Connection command
 * the UI uses. Repeated stories reuse the exact active credential instead of
 * creating parallel gcloud sources. */
export async function configureLiveModel(page) {
  const project = process.env.GEMINI_PROJECT ?? "";
  const location = process.env.GEMINI_LOCATION ?? "global";
  if (!project) throw new Error("this runtime-effect story requires GEMINI_PROJECT");
  const provider = "vertex";
  const endpoint = "vertex-endpoint";
  const host = location === "global" ? "aiplatform.googleapis.com" : `${location}-aiplatform.googleapis.com`;
  const baseUrl = `https://${host}/v1/projects/${project}/locations/${location}/`;

  const credentials = await (await page.request.get(`${BACKEND}/v1/config/credentials?workspace_id=wrkspc_default`)).json();
  const existing = credentials.find(
    (source) =>
      source.provider_id === provider &&
      source.kind === "oauth" &&
      source.oauth_helper === "gcloud" &&
      source.status === "active",
  );
  const connected = await page.request.post(`${BACKEND}/v1/config/provider-connections`, {
    data: {
      workspace_id: "wrkspc_default",
      provider_id: provider,
      display_name: "Vertex AI",
      endpoint_id: endpoint,
      dialect: "vertex_gemini",
      base_url: baseUrl,
      timeout_secs: 60,
      ...(existing
        ? { credential_source_id: existing.id }
        : { oauth_helper: "gcloud" }),
    },
  });
  if (!connected.ok()) {
    throw new Error(
      `could not connect Vertex through Provider Connections: ${connected.status()} ${await connected.text()}`,
    );
  }
  const connection = await connected.json();
  if (!connection.sync || connection.sync.discovered < 1) {
    throw new Error("Vertex Provider Connection returned no discoverable models");
  }
}

/** Deterministic non-provider fixture for stories that do not claim real model
 * connectivity. This is the sole low-level Catalog fixture author; product UI,
 * live recording helpers, and smoke callers use Provider Connections. */
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
