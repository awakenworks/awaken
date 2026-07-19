// Runtime/protocol proof: persist an ACP-backed environment with a locked-down
// sandbox and verify both the managed API projection and visible table.

const ENV_NAME = `recording-acp-sandbox-${Date.now()}`;

export async function run({ page, goto, intro, say, clearCaption, checkpoint, aha, expect, click, type, wait }) {
  await goto("/w/default/environments");
  await intro(
    "Choose how an agent executes without baking a specific CLI or isolation strategy into the agent.",
    "Bind a managed environment to Native or ACP, then apply an explicit sandbox preset that the worker must enforce.",
  );

  await click(page.getByRole("button", { name: /New environment|新建环境/ }));
  await wait(500);
  const modal = page.locator(".modal");
  await type(modal.getByPlaceholder("claude-sandbox-github"), ENV_NAME);
  await say("The protocol adapter is runtime configuration: this environment delegates to Claude Code through ACP.", 4200);
  await click(modal.getByRole("button", { name: "Claude Code", exact: true }));

  await say("Sandbox policy is also data. Locked-down means namespace isolation, no egress, read-only input, and limits.", 4400);
  await click(modal.locator('input[type="checkbox"]'));
  await wait(500);
  await click(modal.getByRole("button", { name: "Locked-down", exact: true }));
  await wait(800);
  await click(modal.getByRole("button", { name: /Create|创建/, exact: true }));
  await wait(1200);

  await checkpoint("the managed environment persists ACP identity and locked-down sandbox policy", async () => {
    const response = await page.request.get("http://127.0.0.1:38080/v1/environments");
    expect(response.ok()).toBeTruthy();
    const body = await response.json();
    const environment = body.data.find((item) => item.name === ENV_NAME);
    expect(environment.config.runtime).toBe("acp:claude");
    expect(environment.config.sandbox.network.mode).toBe("none");
    const row = page.locator("tr").filter({ hasText: ENV_NAME });
    await expect(row).toContainText("Claude Code · ACP");
    await expect(row).toContainText("cloud");
    await expect(row).toContainText("namespace · no egress");
  });

  await clearCaption();
  await aha("The agent stays portable: execution protocol, placement, and containment are replaceable configuration.");
  await wait(1000);
  await clearCaption();
}
