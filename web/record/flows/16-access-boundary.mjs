// IAM proof: mint a scoped token, prove it works, revoke it, then prove denial.

const ADMIN_TOKEN = process.env.AWAKEN_RECORD_ADMIN_TOKEN ?? "";
const CLIENT_NAME = `recording-client-${Date.now()}`;

export const story = {
  promise: "Issue a workspace-scoped client token, use it successfully, then revoke it and prove access stops immediately.",
  effect: "The cleartext appears once, authorizes a protected read, disappears on revoke, and the same credential is then denied.",
  aha: "Access is an explicit lifecycle: issue once, scope every call, revoke immediately—without exposing stored credentials.",
  loyalty: "Predictable credential lifecycle and fail-closed revocation build the trust required for long-running integrations.",
  satisfaction: "One-time copy feedback and an immediate denial test remove ambiguity from security administration.",
  advocacy: "The same token visibly moving from allowed to denied is a strong governance proof for enterprise evaluators.",
};

export async function run({ page, ctx, goto, intro, say, clearCaption, checkpoint, aha, expect, click, type, wait }) {
  if (!ADMIN_TOKEN) throw new Error("16-access-boundary requires AWAKEN_RECORD_ADMIN_TOKEN and an embedded-IAM host");
  await ctx.addInitScript((token) => localStorage.setItem("awaken.console.token", token), ADMIN_TOKEN);
  await goto("/w/default/access");
  await intro(
    "Give an integration only the access it needs and prove revocation takes effect immediately.",
    "Mint a scoped token through embedded IAM, expose cleartext once, verify a protected call, then revoke and deny it.",
  );
  await type(page.getByLabel(/Name|名称/), CLIENT_NAME);
  await page.getByLabel(/Role|角色/).selectOption("workspace_restricted_developer");
  await click(page.getByRole("button", { name: /Mint|铸造/, exact: true }));
  const secretBanner = page.locator(".banner.warn").filter({ hasText: /shown ONCE|只显示一次/ });
  const minted = (await secretBanner.locator("code").innerText()).trim();
  await checkpoint("the one-time token authorizes a protected workspace read", async () => {
    await expect(secretBanner).toBeVisible();
    const response = await page.request.get("http://127.0.0.1:38080/v1/config/catalog", {
      headers: { authorization: `Bearer ${minted}` },
    });
    expect(response.ok()).toBeTruthy();
  });

  await say("Revoke the exact client identity, then replay the protected request with the old token.", 3800);
  const row = page.locator("tr").filter({ hasText: CLIENT_NAME });
  await click(row.getByRole("button", { name: /Revoke|吊销/, exact: true }));
  await checkpoint("revocation removes the identity and the old token is denied", async () => {
    await expect(row).toBeHidden();
    const response = await page.request.get("http://127.0.0.1:38080/v1/config/catalog", {
      headers: { authorization: `Bearer ${minted}` },
    });
    expect([401, 403]).toContain(response.status());
  });
  await clearCaption();
  await aha(story.aha);
  await wait(900);
  await clearCaption();
}
