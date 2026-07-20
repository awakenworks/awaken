// Runtime/protocol proof: persist an ACP-backed environment with a locked-down
// sandbox and verify both the managed API projection and visible table.

const ENV_NAME = `recording-acp-sandbox-${Date.now()}`;

export const story = {
  promise: "Move one Agent onto Claude Code inside a no-egress sandbox without changing the Agent definition.",
  effect: "A Managed session visibly inherits the environment's acp:claude runtime and locked-down sandbox selection.",
  aha: "The Agent stays unchanged while its Managed session switches to Claude Code inside a no-egress sandbox.",
  loyalty: "Replaceable execution environments protect prior Agent investment and reduce platform lock-in anxiety.",
  satisfaction: "The session-level runtime badge confirms that environment configuration reached the execution boundary.",
  advocacy: "The unchanged-Agent portability contrast gives infrastructure and security teams a reason to recommend Awaken.",
};

export async function run({ page, goto, intro, say, clearCaption, checkpoint, aha, expect, click, type, wait }) {
  let createdEnvironment;
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
    createdEnvironment = environment;
    expect(environment.config.runtime).toBe("acp:claude");
    expect(environment.config.sandbox.network.mode).toBe("none");
    const row = page.locator("tr").filter({ hasText: ENV_NAME });
    await expect(row).toContainText("Claude Code · ACP");
    await expect(row).toContainText("cloud");
    await expect(row).toContainText("namespace · no egress");
  });

  const session = await (await page.request.post("http://127.0.0.1:38080/v1/sessions", {
    data: {
      agent: "default",
      environment_id: createdEnvironment.id,
      title: "Portable Agent · locked ACP",
      metadata: { "awaken.runtime": createdEnvironment.config.runtime },
    },
  })).json();
  await goto(`/w/default/sessions/${session.id}`);
  await say("The environment now reaches a real Managed session; runtime provenance is visible beside the Agent.", 3800);
  await checkpoint("the session inherits the environment's ACP runtime selection", async () => {
    await expect(page.getByText("acp:claude", { exact: true })).toBeVisible();
    expect(session.agent.id).toBe("default");
    expect(session.environment_id).toBe(createdEnvironment.id);
    expect(session.metadata["awaken.runtime"]).toBe("acp:claude");
  });

  await clearCaption();
  await aha(story.aha);
  await wait(1000);
  await clearCaption();
}
