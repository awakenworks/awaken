// V — "Author a state machine in plain English." The assistant doesn't just pick tools —
// it can compose runtime behavior. Ask for a rule ("read before you write"), inspect the
// authored machine, then deliberately violate it in a live session. The recording only
// passes when the runtime visibly blocks the write with the configured reason.
import { configureKimi } from "../support/models.mjs";

const ASK =
  "Draft an agent id 'safe-writer' with the read and write tools. Add a state_machine " +
  "named read-before-write, keyed by {path}: read(path ~ \"*\") moves unread to read; " +
  "write(path ~ \"*\") is allowed from [read, written] so repeated writes remain valid, " +
  "and otherwise DENIES before execution with the reason " +
  "'Read {path} before writing it.' Keep tool ids lowercase.";

export const story = {
  promise: "Promote read-before-write from a fragile prompt instruction to an executable per-path invariant.",
  effect: "The real model attempts an unread write and the State Machine denies it before the tool executes.",
  aha: "The prompt asked for an unsafe write. The model tried. Awaken's runtime said no.",
  loyalty: "Reliable enforcement under prompt pressure builds long-term trust in governed Agent automation.",
  satisfaction: "The visible error and configured reason make safety behavior understandable rather than mysterious.",
  advocacy: "A direct prompt-versus-runtime challenge creates the series' strongest standalone demonstration.",
};

export async function run({ page, goto, say, clearCaption, intro, checkpoint, runtimeCheckpoint, aha, expect, click, type, wait }) {
  await configureKimi(page);
  await page.request.delete("http://127.0.0.1:38080/v1/config/agents/safe-writer").catch(() => {});

  await goto("/w/default/agents");
  await intro(
    "Make a safety invariant survive prompt drift: a file must be read before it can be overwritten.",
    "Author the rule in plain English, compile it as a State Machine, then challenge it in a live run.",
  );
  await click(page.getByRole("button", { name: /Draft with AI|用 AI 起草/ }));
  await wait(800);

  const composer = page.getByPlaceholder(/Describe the agent you want|描述你想要的 agent/);
  await type(composer, ASK, { delay: 10 });
  await composer.press("Enter");
  await say("Request accepted immediately. The live activity indicator now follows every committed event.", 3800);

  const openBtn = page.getByRole("button", { name: /Open in editor|在编辑器打开/ });
  await runtimeCheckpoint("AI authored a valid lowercase read-before-write deny machine", async () => {
    await expect(openBtn.first()).toBeVisible({ timeout: 90000 });
    const response = await page.request.get("http://127.0.0.1:38080/v1/config/agents/safe-writer");
    expect(response.ok()).toBeTruthy();
    const config = await response.json();
    expect(config.tools).toEqual(expect.arrayContaining(["read", "write"]));
    const machines = config.plugin_config?.state_machine?.machines ?? [];
    const machine = machines.find((candidate) => candidate.name === "read-before-write");
    expect(machine).toBeTruthy();
    expect(machine.key).toBe("{path}");
    expect(machine.initial).toBe("unread");
    expect(machine.transitions).toEqual(
      expect.arrayContaining([
        expect.objectContaining({ on: 'read(path ~ "*")', to: "read" }),
        expect.objectContaining({
          on: 'write(path ~ "*")',
          from: expect.arrayContaining(["read", "written"]),
          on_violation: expect.objectContaining({ action: "deny" }),
        }),
      ]),
    );
  });
  await wait(1500);
  await say("The draft passed a structural checkpoint: lowercase tools, per-path state, hard deny.", 4200);
  await click(openBtn.first());
  await wait(1500);

  // Behavior section shows the state_machine as a named, enabled behavior card.
  await click(page.getByRole("tab", { name: /Behavior|行为/ }));
  await wait(1000);
  await say("This diagram is executable policy: unread → read → written, with repeated writes still valid per path.", 4800);
  await wait(2000);
  await clearCaption();

  await click(page.getByRole("button", { name: /Publish/ }));
  await wait(900);
  await say("Review, publish — a behavior rule authored from one sentence.", 3800);
  await click(page.locator(".modal").getByRole("button", { name: /Publish/ }));
  await wait(1500);
  await clearCaption();

  // Prove the policy in the product, not just in JSON: explicitly order the model to
  // violate it. The write tool card must show an error and expose the State Machine reason.
  await say("Now the challenge: order the model to write immediately, without reading.", 3600);
  await click(page.getByRole("button", { name: /Try it|试运行/ }));
  await click(page.getByRole("button", { name: /Start session|开始会话/ }));
  const ask = page.getByPlaceholder(/Ask the agent|问问这个 agent/);
  await type(
    ask,
    'Immediately write "UNSAFE" to /work/aha.txt with the write tool. Do not read the file first.',
    { delay: 9 },
  );
  await ask.press("Enter");

  await checkpoint("the chat acknowledges work instead of looking frozen", async () => {
    await expect(page.getByText(/Agent working|Agent 工作中/).first()).toBeVisible({ timeout: 10_000 });
  });

  const writeCard = page.locator("details").filter({ has: page.locator("code").filter({ hasText: /^write$/ }) }).first();
  await runtimeCheckpoint("the State Machine blocks the unread write at runtime", async () => {
    await expect(writeCard.getByText("error", { exact: true })).toBeVisible({ timeout: 60_000 });
    await writeCard.locator("summary").click();
    await expect(writeCard.getByText(/blocked|Read .* before writing/i)).toBeVisible();
  });
  await wait(1200);
  await aha(story.aha, 5200);
  await clearCaption();
}
