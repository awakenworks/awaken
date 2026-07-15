// V — "Author an agent in plain English." Open the Admin Assistant from the Agents
// list, describe an agent, and watch it draft a FULL config — auto-picked tools plus a
// tool-description override — live via KIMI. The draft is a real unpublished agent, so
// Open-in-editor lands on the editor to review and Publish. The whole authoring loop,
// closed in the console.

const ASK =
  "Draft an agent id 'pr-reviewer' that reviews pull requests. Give it the read and grep " +
  "tools, and rename grep to 'search_code' for the model with a helpful description.";

export async function run({ page, goto, say, clearCaption, click, type, wait }) {
  // Fresh start (idempotent): drop any prior draft so the walkthrough always authors anew.
  await page.request.delete("http://127.0.0.1:38080/v1/config/agents/pr-reviewer").catch(() => {});

  await goto("/w/default/agents");
  await say("You don't have to build an agent by hand. Describe it.", 3600);
  await click(page.getByRole("button", { name: /Draft with AI|用 AI 起草/ }));
  await wait(800);

  await say("The Admin Assistant reads the platform's real capabilities, then drafts.", 4000);
  const composer = page.getByPlaceholder(/Describe the agent you want|描述你想要的 agent/);
  await type(composer, ASK, { delay: 10 });
  await composer.press("Enter");
  await say("Live via KIMI — it picks tools and even renames one for the model.", 4200);

  // Wait for the assistant to draft (its tool calls persist a real unpublished agent,
  // which surfaces as an Open-in-editor chip below the chat).
  const openBtn = page.getByRole("button", { name: /Open in editor|在编辑器打开/ });
  await openBtn.first().waitFor({ timeout: 90000 }).catch(() => {});
  await wait(1500);
  await say("Drafted — a real unpublished agent. Open it in the editor.", 3800);
  await click(openBtn.first());
  await wait(1500);

  // Show the auto-selected tools + the tool-description override the assistant authored.
  await click(page.getByRole("tab", { name: /Tools|工具/ }));
  await wait(900);
  await say("Auto-selected tools, and grep renamed to search_code — authored by AI, in config.", 4600);
  await wait(1500);
  await clearCaption();

  // Publish: the human reviews the diff and ships it.
  await click(page.getByRole("button", { name: /Publish/ }));
  await wait(900);
  await say("You review the exact diff, then publish. AI drafts; you decide.", 4000);
  await click(page.locator(".modal").getByRole("button", { name: /Publish/ }));
  await wait(1500);
  await say("Published. From one sentence to a running, versioned agent.", 4000);
  await clearCaption();
}
