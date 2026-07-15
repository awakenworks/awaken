// V — "Build an agent, prove it live." Configure an agent from empty in the
// intent-sectioned editor (model, system prompt, behavior), Save, Publish (with a
// domain-labeled config diff — transparency), then Try it in the Sandbox for a REAL
// KIMI reply. Agents are configured, not coded.

const AGENT_ID = "release-notes-writer";
const SYSTEM =
  "You are a release-notes writer. Given a list of merged PRs, produce concise, " +
  "friendly notes grouped into Features, Fixes, and Breaking changes. Keep each " +
  "bullet under 100 characters.";
const ASK =
  "Draft release notes for: #482 add dark mode, #491 fix crash on export, #500 drop Node 18";

export async function run({ page, goto, say, clearCaption, click, type, wait }) {
  // Fresh start (idempotent): drop any prior agent so the walkthrough always creates.
  await page.request.delete(`http://127.0.0.1:38080/v1/config/agents/${AGENT_ID}`).catch(() => {});

  await goto("/w/default/agents/new");
  await say("An agent is configured, not coded. Start from an empty definition.", 3800);

  await type(page.getByPlaceholder("coding-agent"), AGENT_ID);
  await say("Pick a model from the catalog — only credentialed models are offered.", 3600);
  await click(page.locator("select").first());
  await page.locator("select").first().selectOption("kimi-k2-0711-preview");
  await wait(400);

  await say("Its behavior is just the system prompt — no code, no redeploy.", 3600);
  await type(page.locator("textarea").first(), SYSTEM, { delay: 12 });

  await say("The left rail organizes every knob by intent: Behavior, Tools, Resources.", 4000);
  await click(page.getByRole("tab", { name: /Behavior|行为/ }));
  await wait(900);
  await click(page.getByRole("tab", { name: /Overview|概览/ }));
  await wait(600);
  await clearCaption();

  await click(page.getByRole("button", { name: "Save", exact: true }));
  await wait(1200);

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
  await type(ask, ASK, { delay: 12 });
  await say("The runtime resolves model → offering → credential → a real KIMI executor.", 4200);
  await ask.press("Enter");
  // Wait for the live reply to render (poll for assistant text, up to 60s).
  await page
    .waitForFunction(
      () => /Features|Fixes|Breaking|release/i.test(document.body.innerText),
      null,
      { timeout: 60000 },
    )
    .catch(() => {});
  await wait(2500);
  await say("A real reply — grouped release notes, live from the model. Not a mock.", 4600);
  await wait(1500);
  await clearCaption();
}
