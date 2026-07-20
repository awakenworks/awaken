// V — "Build an agent, prove it live." Configure an agent from empty in the
// intent-sectioned editor (model, system prompt, behavior), Save, Publish (with a
// domain-labeled config diff — transparency), then Try it in the Sandbox for a REAL
// KIMI reply. Agents are configured, not coded.
import { configureKimi } from "../support/models.mjs";

const AGENT_ID = "release-notes-writer";
const SYSTEM =
  "You are a release-notes writer. Given a list of merged PRs, produce concise, " +
  "friendly notes grouped into Features, Fixes, and Breaking changes. Keep each " +
  "bullet under 100 characters.";
const ASK_HEAD = "Draft release notes from these merged PRs:";
const ASK_DETAIL = "#482 dark mode · #491 export crash · #500 drop Node 18";

export const story = {
  promise: "Convert a recurring release-note task into a versioned specialist and verify its output in one workflow.",
  effect: "Publish automatically saves and validates the Draft, then the live Agent returns structured release notes.",
  aha: "One config became a live, versioned specialist—and the answer on screen is from the real model.",
  loyalty: "A repeatable Draft-to-proof workflow gives teams a dependable reason to build their next specialist here.",
  satisfaction: "Automatic validation and an immediate Sandbox result minimize setup friction and uncertainty.",
  advocacy: "The before-and-after transformation from three PR titles to polished notes is naturally shareable.",
};

export async function run({ page, goto, say, clearCaption, intro, checkpoint, runtimeCheckpoint, aha, expect, click, type, wait }) {
  await configureKimi(page);
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

  await say("Publish automatically saves and validates the Draft before asking for the only confirmation.", 4000);
  await click(page.getByRole("button", { name: /Publish/ }));
  const publishModal = page.locator(".modal");
  await checkpoint("the Draft is saved, compiled, and previewed before publication", async () => {
    await expect(publishModal.getByText(/Draft compiled successfully|草稿已通过编译/)).toBeVisible();
    const response = await page.request.get(`http://127.0.0.1:38080/v1/config/agents/${AGENT_ID}`);
    expect(response.ok()).toBeTruthy();
    const config = await response.json();
    expect(config.id).toBe(AGENT_ID);
    expect(config.model?.id).toBe("kimi-for-coding");
  });
  await say("The exact domain-labeled diff is visible; this is the one decision the user owns.", 4000);
  await click(publishModal.getByRole("button", { name: /Publish/ }));
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
  const agentReply = page.locator(".card").filter({ hasText: "⬡ agent" }).last();
  await runtimeCheckpoint("the published agent returns structured release notes from the live model", async () => {
    await expect(agentReply).toContainText(/Features|Fixes|Breaking changes/i, { timeout: 60_000 });
  });
  await wait(2500);
  await aha(story.aha);
  await wait(1500);
  await clearCaption();
}
