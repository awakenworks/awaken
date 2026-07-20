const BACKEND = "http://127.0.0.1:38080";

export async function configureKimi(page) {
  const key = process.env.KIMI_KEY ?? process.env.ANTHROPIC_API_KEY ?? "";
  const upstreamModel = process.env.KIMI_MODEL ?? process.env.ANTHROPIC_MODEL ?? "kimi-for-coding";
  if (!key) throw new Error("this runtime-effect story requires KIMI_KEY or ANTHROPIC_API_KEY");
  await page.request.put(`${BACKEND}/v1/config/providers/kimi`, {
    data: { id: "kimi", slug: "kimi", display_name: "Kimi", version: 1 },
  });
  await page.request.put(`${BACKEND}/v1/config/endpoints/kimi-ep`, {
    data: { id: "kimi-ep", provider_id: "kimi", dialect: "anthropic_messages", base_url: "https://api.kimi.com/coding/v1/", timeout_secs: 60, display_name: "Kimi", version: 1 },
  });
  await page.request.post(`${BACKEND}/v1/config/offerings`, {
    data: { model_id: "kimi-for-coding", provider_id: "kimi", protocol_endpoint_id: "kimi-ep", dialect: "anthropic_messages", upstream_model: upstreamModel },
  });
  await page.request.post(`${BACKEND}/v1/config/credentials`, {
    data: { workspace_id: "wrkspc_default", kind: "vault", provider_id: "kimi", secret: key },
  });
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
