// V — "Author a state machine in plain English." The assistant doesn't just pick tools —
// it can compose runtime behavior. Ask for a rule ("read before you write") and it drafts
// a state_machine plugin, complete with a system-reminder emit — validated and persisted
// as a real agent you open, inspect, and publish.

const ASK =
  "Draft an agent id 'safe-writer' with the read and write tools, and add a state_machine " +
  "that requires reading a file before writing it, emitting a system reminder if it writes " +
  "without reading first.";

export async function run({ page, goto, say, clearCaption, click, type, wait }) {
  await page.request.delete("http://127.0.0.1:38080/v1/config/agents/safe-writer").catch(() => {});

  await goto("/w/default/agents");
  await say("Configurability goes deeper than tools — describe a runtime rule.", 3800);
  await click(page.getByRole("button", { name: /Draft with AI|用 AI 起草/ }));
  await wait(800);

  const composer = page.getByPlaceholder(/Describe the agent you want|描述你想要的 agent/);
  await type(composer, ASK, { delay: 10 });
  await composer.press("Enter");
  await say("It reads the platform's plugins, then composes a state machine — live.", 4200);

  const openBtn = page.getByRole("button", { name: /Open in editor|在编辑器打开/ });
  await openBtn.first().waitFor({ timeout: 90000 }).catch(() => {});
  await wait(1500);
  await say("Drafted a state_machine plugin — read-before-write, enforced at runtime.", 4200);
  await click(openBtn.first());
  await wait(1500);

  // Behavior section shows the state_machine as a named, enabled behavior card.
  await click(page.getByRole("tab", { name: /Behavior|行为/ }));
  await wait(1000);
  await say("The rule is a plugin the AI authored — with a system reminder it emits on violation.", 4800);
  await wait(2000);
  await clearCaption();

  await click(page.getByRole("button", { name: /Publish/ }));
  await wait(900);
  await say("Review, publish — a behavior rule authored from one sentence.", 3800);
  await click(page.locator(".modal").getByRole("button", { name: /Publish/ }));
  await wait(1500);
  await clearCaption();
}
