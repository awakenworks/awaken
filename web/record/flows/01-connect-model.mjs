// V — "Connect a model." Declare a provider + endpoint + offering in the Models
// surface, then seal its credential in the Credentials surface. Everything through
// the console UI — no env vars, no secrets in code (the platform's ironclad rule).
// The model here is KIMI (Moonshot), reached over its Anthropic-compatible endpoint.

const KEY = process.env.KIMI_KEY ?? "";

export async function run({ page, goto, say, clearCaption, click, type, wait }) {
  await goto("/w/default/models");
  await say("Every model the platform can run is declared here — in the console, never in env vars.", 4200);
  await clearCaption();

  const authorCard = page.locator(".card").filter({ hasText: "Author provider" });
  const inputs = authorCard.locator("input.input");
  await say("Declare the provider, its Anthropic-compatible endpoint, and the offering.", 3600);
  await type(inputs.nth(0), "kimi"); // Provider
  await type(inputs.nth(1), "kimi-ep"); // Endpoint id
  await type(inputs.nth(2), "https://api.kimi.com/coding/v1/"); // base_url
  await type(page.getByPlaceholder("model-id"), "kimi-k2-0711-preview");
  await say("The context window feeds the compaction budget — published as a model attribute.", 3800);
  await type(page.getByPlaceholder("200000"), "262144"); // Context window
  await click(authorCard.getByRole("button", { name: /Author|写入/ }));
  await wait(1200);

  await say("The catalog now carries the model — 262k context and all.", 3400);
  await clearCaption();

  await goto("/w/default/credentials");
  await say("A model can't run until it has a credential. Supply-side, sealed, write-only.", 4000);
  await click(page.getByRole("button", { name: /Enter credential|录入凭证/ }));
  await wait(500);
  const modal = page.locator(".modal");
  const providerField = modal.locator("input.input").first();
  await type(providerField, "kimi");
  if (KEY) {
    await type(modal.locator('input[type="password"]'), KEY, { delay: 6 });
    await say("The console never reads a stored secret back — it only ever writes one in.", 4000);
    await click(modal.getByRole("button", { name: /Seal & save|密封保存/ }));
    await wait(1500);
    await say("Sealed. The model is now runnable — provider, endpoint, offering, and key.", 3800);
  } else {
    await say("(No key supplied — the seal step is shown but skipped.)", 3000);
  }
  await clearCaption();
}
