// IAM proof: mint a scoped token, prove it works, revoke it, then prove denial.
import { createRequire } from "node:module";
import { resolve } from "node:path";
import { BACKEND } from "../support/control-plane.mjs";

const CLIENT_NAME = `recording-client-${Date.now()}`;
const SDK_CLIENT_NAME = `managed-sdk-client-${Date.now()}`;
const requireFromE2e = createRequire(resolve(import.meta.dirname, "../../../e2e/package.json"));
const Anthropic = requireFromE2e("@anthropic-ai/sdk").default;
const BETAS = ["managed-agents-2026-04-01"];

async function requestWithOnlyServiceKey(token) {
  // Deliberately use Node's fetch rather than the Playwright request context. It
  // context shares the browser's local-admin cookie, which could make this proof
  // pass even when the newly minted service key is invalid or already revoked.
  return fetch(`${BACKEND}/v1/config/catalog`, {
    headers: { authorization: `Bearer ${token}` },
  });
}

export async function run({ page, goto, intro, say, clearCaption, checkpoint, aha, expect, click, type, wait }) {
  if (!process.env.AWAKEN_RECORD_SETUP_TOKEN?.trim()) {
    throw new Error("16-access-boundary requires the one-time AWAKEN_RECORD_SETUP_TOKEN from a self-managed all-in-one host");
  }
  await goto("/w/default/access");
  await intro(
    "A revoked integration key must not read the workspace again.",
    "Issue a scoped test key, use it once, revoke it, then replay the exact protected request.",
  );
  await type(page.getByLabel(/Service principal|服务主体/), CLIENT_NAME);
  await page.getByLabel(/Role|角色/).selectOption("workspace_restricted_developer");
  await click(page.getByRole("button", { name: /Create key|创建 Key/, exact: true }));
  const secretBanner = page.locator(".banner.warn").filter({ hasText: /shown ONCE|只显示一次/ });
  const minted = (await secretBanner.locator("code").innerText()).trim();
  await checkpoint("the one-time token authorizes a protected workspace read", async () => {
    await expect(secretBanner).toBeVisible();
    const response = await requestWithOnlyServiceKey(minted);
    expect(response.status).toBe(200);
  });

  await say("Revoke the client, then retry the same request with the old key.", 3400);
  const row = page.locator("tr").filter({ hasText: CLIENT_NAME });
  await click(row.getByRole("button", { name: /Revoke|吊销/, exact: true }));
  await click(page.getByRole("alertdialog").getByRole("button", { name: /Revoke|吊销/, exact: true }));
  await checkpoint("revocation removes the identity and the old token is denied", async () => {
    await expect(row.getByText(/revoked|已吊销/, { exact: true })).toBeVisible();
    await expect(secretBanner).toBeHidden();
    const response = await requestWithOnlyServiceKey(minted);
    expect([401, 403]).toContain(response.status);
  });

  // The read-only proof above protects least privilege. This second branch
  // closes the developer quickstart itself: the role named by Access must be
  // able to create a real Managed Session through the official SDK, and the
  // same credential must stop working immediately after UI revocation.
  await type(page.getByLabel(/Service principal|服务主体/), SDK_CLIENT_NAME);
  await page.getByLabel(/Role|角色/).selectOption("workspace_admin");
  await click(page.getByRole("button", { name: /Create key|创建 Key/, exact: true }));
  const sdkSecretBanner = page.locator(".banner.warn").filter({ hasText: /shown ONCE|只显示一次/ });
  const sdkKey = (await sdkSecretBanner.locator("code").innerText()).trim();
  const sdk = new Anthropic({ apiKey: sdkKey, baseURL: BACKEND, maxRetries: 0 });
  const session = await sdk.beta.sessions.create({
    agent: "assistant",
    environment_id: "env_local",
    title: "Service key SDK proof",
    betas: BETAS,
  });
  await checkpoint("the UI-created service key starts and reads one real SDK Session", async () => {
    expect(session.id).toMatch(/^sesn_/);
    const retrieved = await sdk.beta.sessions.retrieve(session.id, { betas: BETAS });
    expect(retrieved.id).toBe(session.id);
  });

  await goto(`/w/default/sessions/${session.id}`);
  await checkpoint("Console opens the exact Session created by the official SDK", async () => {
    await expect(page.getByText("Service key SDK proof", { exact: true })).toBeVisible();
    const technicalId = page.locator("details.technical-id").filter({ hasText: session.id });
    await technicalId.locator("summary").click();
    await expect(technicalId.getByText(session.id, { exact: true })).toBeVisible();
  });

  await goto("/w/default/access");
  const sdkRow = page.locator("tr").filter({ hasText: SDK_CLIENT_NAME });
  await click(sdkRow.getByRole("button", { name: /Revoke|吊销/, exact: true }));
  await click(page.getByRole("alertdialog").getByRole("button", { name: /Revoke|吊销/, exact: true }));
  await checkpoint("revoking the UI-created key denies the same official SDK client", async () => {
    await expect(sdk.beta.sessions.retrieve(session.id, { betas: BETAS })).rejects.toMatchObject({ status: 401 });
  });
  await clearCaption();
  await wait(900);
  await clearCaption();
}
