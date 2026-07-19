// The series opener: one evidence-backed sweep across the control plane. It shows
// the configuration model before the deep-dive videos prove each capability.

export async function run({ page, goto, intro, say, clearCaption, checkpoint, aha, expect, click, wait }) {
  await goto("/w/default/overview");
  await intro(
    "Control every consequential agent capability from one inspectable configuration plane.",
    "Awaken unifies models, prompts, memory, tools, protocols, execution environments, policy, and observable runs.",
  );

  await say("Start with the supply chain: provider, endpoint, offering, and sealed inference credential.", 3800);
  await click(page.getByText("Models", { exact: true }));
  await wait(700);

  await say("Agent configuration keeps behavior, tools, resources, memory, compaction, continuation, and reminders together.", 4400);
  await click(page.getByText("Agents", { exact: true }));
  await wait(700);

  await say("Execution is independent: Native or ACP adapters, managed sessions, MCP tools, and explicit sandbox policy.", 4400);
  await click(page.getByText("Environments", { exact: true }));
  await wait(800);

  await checkpoint("the running backend advertises native, ACP, sandbox, tools, and configurable plugins", async () => {
    const response = await page.request.get("http://127.0.0.1:38080/v1/capabilities");
    expect(response.ok()).toBeTruthy();
    const caps = await response.json();
    expect(caps.runtimes.some((runtime) => runtime.id === "awaken")).toBeTruthy();
    expect(caps.runtimes.some((runtime) => runtime.id.startsWith("acp:"))).toBeTruthy();
    expect(caps.sandbox.presets.length).toBeGreaterThan(0);
    expect(caps.tools.length).toBeGreaterThan(0);
    expect(caps.plugins.some((plugin) => plugin.id === "state_machine")).toBeTruthy();
  });

  await say("Every claim in this series ends with a live checkpoint, so configuration and runtime behavior stay connected.", 4200);
  await clearCaption();
  await click(page.getByRole("button", { name: /New environment|新建环境/ }));
  await wait(700);
  await aha("One control plane configures the agent; capability contracts prove what the selected runtime can enforce.");
  await wait(1000);
  await clearCaption();
}
