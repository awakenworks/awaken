// V — "Connect a model." Declare a provider + endpoint + offering in the Models
// surface, then seal its credential in the Credentials surface. Everything through
// the console UI — no env vars, no secrets in code (the platform's ironclad rule).
// The model here is KIMI (Moonshot), reached over its Anthropic-compatible endpoint.

const KEY = process.env.KIMI_KEY ?? process.env.ANTHROPIC_API_KEY ?? "";
const MODEL = process.env.KIMI_MODEL ?? process.env.ANTHROPIC_MODEL ?? "kimi-for-coding";

export const story = {
  promise: "Connect KIMI without putting its credential in source code and prove that it can answer immediately.",
  effect: "A sealed provider credential resolves the catalog model and produces a visible live-model response.",
  aha: "A catalog entry plus a write-only secret becomes a verified model response—without exposing the key.",
  loyalty: "Transparent supply and write-only secrets build confidence that repeated model operations stay governable.",
  satisfaction: "The story ends with an answer, removing uncertainty about whether model setup actually worked.",
  advocacy: "A live response beside a never-readable credential creates a strong security-and-speed proof to share.",
};

export async function run({ page, goto, say, clearCaption, intro, checkpoint, runtimeCheckpoint, aha, expect, click, type, wait }) {
  if (!KEY) throw new Error("01-connect-model requires KIMI_KEY or ANTHROPIC_API_KEY so the story can end with a real response");
  await goto("/w/default/models");
  await intro(
    "Make a model usable without leaking credentials into code or deployment manifests.",
    "Declare supply in the model catalog, then seal its credential through a write-only control plane.",
  );

  const authorCard = page.locator(".card").filter({ hasText: "Author provider" });
  const inputs = authorCard.locator("input.input");
  await say("Declare the provider, its Anthropic-compatible endpoint, and the offering.", 3600);
  await type(inputs.nth(0), "kimi"); // Provider
  await type(inputs.nth(1), "kimi-ep"); // Endpoint id
  await type(inputs.nth(2), "https://api.kimi.com/coding/v1/"); // base_url
  await type(page.getByPlaceholder("model-id"), MODEL);
  await say("The context window feeds the compaction budget — published as a model attribute.", 3800);
  await type(page.getByPlaceholder("200000"), "262144"); // Context window
  await click(authorCard.getByRole("button", { name: /Author|写入/ }));
  await wait(1200);

  await checkpoint("the authored KIMI offering is visible in the catalog", async () => {
    await expect(page.getByText(MODEL).first()).toBeVisible();
  });

  await say("The catalog now carries the model — 262k context and all.", 3400);
  await clearCaption();

  await goto("/w/default/credentials");
  await say("A model can't run until it has a credential. Supply-side, sealed, write-only.", 4000);
  await click(page.getByRole("button", { name: /Enter credential|录入凭证/ }));
  await wait(500);
  const modal = page.locator(".modal");
  const providerField = modal.locator("input.input").first();
  await type(providerField, "kimi");
  await type(modal.locator('input[type="password"]'), KEY, { delay: 6 });
  await say("The console never reads a stored secret back — it only ever writes one in.", 4000);
  await click(modal.getByRole("button", { name: /Seal & save|密封保存/ }));
  await wait(1200);
  await checkpoint("the sealed credential is accepted", async () => {
    await expect(modal).toBeHidden();
    const credentialRow = page.locator("tr").filter({ hasText: "kimi" }).last();
    await expect(credentialRow).toContainText("active");
    const response = await page.request.get(
      "http://127.0.0.1:38080/v1/config/credentials?workspace_id=wrkspc_default",
    );
    expect(response.ok()).toBeTruthy();
    const credentials = await response.json();
    expect(credentials).toEqual(expect.arrayContaining([
      expect.objectContaining({ provider_id: "kimi", status: "active" }),
    ]));
    expect(credentials.some((credential) => "secret" in credential)).toBe(false);
  });

  await goto("/w/default/models");
  const row = page.locator("tr", { hasText: MODEL });
  await say("Setup is not the finish line. Test the exact catalog model and require a real answer.", 3600);
  await click(row.getByRole("button", { name: /Test|测试/ }));
  const composer = page.getByPlaceholder(/Say hello|打个招呼/);
  await type(composer, "Reply with exactly: MODEL READY", { delay: 14 });
  await composer.press("Enter");
  const agentReply = page.locator(".card").filter({ hasText: "⬡ agent" }).last();
  await runtimeCheckpoint("KIMI returns a visible response through the configured provider", async () => {
    await expect(agentReply).toContainText("MODEL READY", { timeout: 60_000 });
  });
  await aha(story.aha);
  await clearCaption();
}
