import http from "node:http";

const BACKEND = "http://127.0.0.1:38080";
const SYNTHETIC_KEY = "recording-fixture-only"; // awaken-allow: secret (synthetic fixture)
let syntheticDirectory;
let syntheticModel = "recording-model";

async function ensureSyntheticDirectory() {
  if (syntheticDirectory) return syntheticDirectory;
  const server = http.createServer((_request, response) => {
    response.writeHead(200, { "content-type": "application/json" });
    response.end(JSON.stringify({
      data: [{ id: syntheticModel }],
      has_more: false,
    }));
  });
  await new Promise((resolve, reject) => {
    server.once("error", reject);
    server.listen(0, "127.0.0.1", resolve);
  });
  server.unref();
  const address = server.address();
  if (!address || typeof address === "string") throw new Error("synthetic model directory did not bind");
  syntheticDirectory = `http://127.0.0.1:${address.port}`;
  return syntheticDirectory;
}

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
      idempotency_key: "record-live-vertex",
      workspace_id: "wrkspc_default",
      provider_id: provider,
      display_name: "Vertex AI",
      dialect: "vertex_gemini",
      configuration: { project_id: project, location },
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

/** Deterministic provider directory for stories that do not claim live model
 * connectivity. It still enters through the exact Provider Connection command. */
export async function configureSyntheticModel(page, id) {
  syntheticModel = id;
  const provider = "anthropic";
  const directory = await ensureSyntheticDirectory();
  const credentials = await (await page.request.get(
    `${BACKEND}/v1/config/credentials?workspace_id=wrkspc_default`,
  )).json();
  const existing = credentials.find(
    (source) => source.provider_id === provider && source.status === "active",
  );
  const response = await page.request.post(`${BACKEND}/v1/config/provider-connections`, {
    data: {
      idempotency_key: `record-synthetic-${id}`,
      workspace_id: "wrkspc_default",
      provider_id: provider,
      display_name: "Recording fixture",
      dialect: "anthropic_messages",
      base_url: `${directory}/v1/`,
      timeout_secs: 60,
      ...(existing ? { credential_source_id: existing.id } : { secret: SYNTHETIC_KEY }),
    },
  });
  if (!response.ok()) {
    throw new Error(`synthetic Provider Connection failed: ${response.status()} ${await response.text()}`);
  }
}
