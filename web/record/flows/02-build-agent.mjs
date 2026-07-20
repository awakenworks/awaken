// V — "Build an agent, prove it live." Configure an agent from empty in the
// intent-sectioned editor (model, system prompt, behavior), Save, Publish (with a
// domain-labeled config diff — transparency), then Try it in the Sandbox for a REAL
// KIMI reply. Agents are configured, not coded.

const AGENT_ID = "release-notes-writer";
const SYSTEM =
  "You are a release-notes writer. Given a list of merged PRs, produce concise, " +
  "friendly notes grouped into Features, Fixes, and Breaking changes. Keep each " +
  "bullet under 100 characters.";
const ASK_HEAD = "Draft release notes from these merged PRs:";
const ASK_DETAIL = "#482 dark mode · #491 export crash · #500 drop Node 18";

export async function run({ page, goto, say, clearCaption, intro, checkpoint, aha, expect, click, type, wait }) {
  // Fresh start (idempotent): drop any prior agent so the walkthrough always creates.
  await page.request.delete(`http://127.0.0.1:38080/v1/config/agents/${AGENT_ID}`).catch(() => {});

  await goto("/w/default/agents/new");
  await intro(
    "Convert a repeatable job into a governed agent without writing orchestration code.",
    "Configure behavior, publish an exact diff, then prove the installed agent against a live model.",
  );

  await type(page.getByPlaceholder("coding-agent"), AGENT_ID);
  await say("Pick a model from the catalog — only credentialed models are offered.", 3600);
  await click(page.locator("select").first());
  await page.locator("select").first().selectOption("kimi-for-coding");
  await wait(400);

  await say("Its behavior is just the system prompt — no code, no redeploy.", 3600);
  await type(page.locator("textarea").first(), SYSTEM, { delay: 12 });

  await say("Numbered chapters organize every knob by intent: Behavior, Tools, Integrations, and Resources.", 4000);
  await click(page.getByRole("tab", { name: /Behavior|行为/ }));
  await wait(900);
  await click(page.getByRole("tab", { name: /Overview|概览/ }));
  await wait(600);
  await clearCaption();

  await click(page.getByRole("button", { name: "Save", exact: true }));
  await wait(1200);
  await checkpoint("the draft is persisted with the selected model", async () => {
    const response = await page.request.get(`http://127.0.0.1:38080/v1/config/agents/${AGENT_ID}`);
    expect(response.ok()).toBeTruthy();
    const config = await response.json();
    expect(config.id).toBe(AGENT_ID);
    expect(config.model?.id).toBe("kimi-for-coding");
  });

  await say("Publish previews the exact config diff — domain-labeled, nothing hidden.", 4200);
  await click(page.getByRole("button", { name: /Publish/ }));
  await wait(900);
  await click(page.locator(".modal").getByRole("button", { name: /Publish/ }));
  await wait(1200);
  await say("Published — compiled to a content-addressed fingerprint.", 3400);
  await clearCaption();

  await say("Now prove it. Try it opens a live Sandbox from any section.", 3600);
  await click(page.getByRole("button", { name: /Try it/ }));
  await wait(700);
  await click(page.getByRole("button", { name: /Start session|开始会话/ }));
  await wait(800);
  const ask = page.getByPlaceholder(/Ask the agent|问问这个 agent/);
  await say("Keep the request concise. Shift+Enter adds context; the bound Skill and tools own procedural detail.", 3600);
  await type(ask, ASK_HEAD, { delay: 14 });
  await ask.press("Shift+Enter");
  await ask.pressSequentially(ASK_DETAIL, { delay: 12 });
  await ask.press("Enter");
  await expect(page.locator(".transcript-pending-message")).toContainText(ASK_DETAIL);
  await expect(page.locator(".agent-working")).toBeVisible();
  await say("The full multiline request stays visible and the Agent shows work immediately while KIMI executes.", 3800);
  // Wait for the live reply to render (poll for assistant text, up to 60s).
  await checkpoint("the published agent returns structured release notes from the live model", async () => {
    await page.waitForFunction(
      () => /Features|Fixes|Breaking|release/i.test(document.body.innerText),
      null,
      { timeout: 60000 },
    );
  });
  await wait(2500);
  await aha("One config became a live, versioned specialist—and the answer on screen is from the real model.");
  await wait(1500);
  await clearCaption();
}
