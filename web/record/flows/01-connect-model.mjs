// V — "Connect a model." Declare a provider + endpoint + offering in the Models
// surface, then seal its credential in the Credentials surface. Everything through
// the console UI — no env vars, no secrets in code (the platform's ironclad rule).
// The model here is KIMI (Moonshot), reached over its Anthropic-compatible endpoint.

const KEY = process.env.KIMI_KEY ?? "";

export async function run({ page, goto, say, clearCaption, intro, checkpoint, aha, expect, click, type, wait }) {
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
  await type(page.getByPlaceholder("model-id"), "kimi-for-coding");
  await say("The context window feeds the compaction budget — published as a model attribute.", 3800);
  await type(page.getByPlaceholder("200000"), "262144"); // Context window
  await click(authorCard.getByRole("button", { name: /Author|写入/ }));
  await wait(1200);

  await checkpoint("the authored KIMI offering is visible in the catalog", async () => {
    await expect(page.getByText("kimi-for-coding").first()).toBeVisible();
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
  if (KEY) {
    await type(modal.locator('input[type="password"]'), KEY, { delay: 6 });
    await say("The console never reads a stored secret back — it only ever writes one in.", 4000);
    await click(modal.getByRole("button", { name: /Seal & save|密封保存/ }));
    await wait(1500);
    await checkpoint("the sealed credential is accepted", async () => {
      await expect(page.locator(".toast").filter({ hasText: /saved|sealed|已保存|密封/i })).toBeVisible();
    });
  } else {
    await say("No key supplied for this recording, so the write-only secret form is left untouched.", 3400);
    await page.keyboard.press("Escape");
    await goto("/w/default/models");
    await checkpoint("the model remains an explicit catalog entry without exposing a secret", async () => {
      await expect(page.getByText("kimi-for-coding").first()).toBeVisible();
    });
  }
  await aha(KEY
    ? "A runnable model is a versioned catalog entry plus a secret the console can write—but never read back."
    : "Model supply is explicit and inspectable; secrets remain outside code.");
  await clearCaption();
}
