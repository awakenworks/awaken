// Deployment proof: configure a standing schedule and mint an auditable run receipt.
import { configureSyntheticModel } from "../support/models.mjs";

const AGENT_ID = "scheduled-report-agent";
const MODEL_ID = "deployment-recording-model";
const DEPLOYMENT_NAME = `Weekly release report · ${Date.now()}`;

export const story = {
  promise: "Convert a published Agent into a standing scheduled operation and trigger it without editing the Agent.",
  effect: "The Deployment persists its Agent, Environment, cron schedule, and visibly reports the triggered deployment-run id.",
  aha: "Automation is a reusable Agent-to-Environment binding, and every trigger receives its own auditable run identity.",
  loyalty: "Standing operations make the platform part of recurring work rather than a tool users must remember to invoke.",
  satisfaction: "A visible run receipt confirms the trigger immediately and removes uncertainty about whether the click worked.",
  advocacy: "The shift from chat to scheduled operation communicates production readiness in one compact transformation.",
};

export async function run({ page, goto, intro, say, clearCaption, checkpoint, aha, expect, click, type, wait }) {
  await configureSyntheticModel(page, MODEL_ID);
  await page.request.put(`http://127.0.0.1:38080/v1/config/agents/${AGENT_ID}`, {
    data: {
      id: AGENT_ID, name: "Scheduled report agent", model: { id: MODEL_ID },
      system: "Prepare the scheduled release report.", tools: [], mcp_servers: [], skills: [],
      plugins: [], plugin_config: {}, context_policy: { kind: "keep_all" }, max_steps: 4,
    },
  });
  const published = await page.request.post(`http://127.0.0.1:38080/v1/config/agents/${AGENT_ID}/publish`);
  expect(published.ok()).toBeTruthy();
  const environmentResponse = await page.request.post("http://127.0.0.1:38080/v1/environments", {
    data: { name: `Scheduled native · ${Date.now()}`, config: { type: "cloud", runtime: "awaken" } },
  });
  expect(environmentResponse.ok()).toBeTruthy();
  const environment = await environmentResponse.json();

  await goto("/w/default/deployments");
  await intro(
    "Move a proven Agent from ad-hoc use into a repeatable scheduled operation.",
    "Bind the published Agent to an Environment and cron schedule, then mint a visible deployment-run receipt.",
  );
  await click(page.getByRole("button", { name: /New deployment|新建部署/ }));
  const modal = page.locator(".modal");
  await type(modal.getByPlaceholder("nightly-report"), DEPLOYMENT_NAME);
  await modal.locator("select").nth(0).selectOption(AGENT_ID);
  await modal.locator("select").nth(1).selectOption(environment.id);
  await type(modal.getByPlaceholder("0 20 * * 5"), "0 9 * * 1");
  await type(modal.getByPlaceholder("UTC"), "Asia/Shanghai");
  await type(modal.locator("textarea"), "Prepare this week's verified release report.");
  await click(modal.getByRole("button", { name: /Create|创建/, exact: true }));

  const row = page.locator("tr").filter({ hasText: DEPLOYMENT_NAME });
  await checkpoint("the standing operation persists its schedule and environment", async () => {
    await expect(row).toContainText("0 9 * * 1");
    const response = await page.request.get("http://127.0.0.1:38080/v1/deployments");
    const body = await response.json();
    expect(body.data).toEqual(expect.arrayContaining([
      expect.objectContaining({ name: DEPLOYMENT_NAME, environment_id: environment.id }),
    ]));
  });

  await say("Run now uses the same standing definition and returns an auditable deployment-run identity.", 4000);
  const responsePromise = page.waitForResponse((response) => response.request().method() === "POST" && /\/v1\/deployments\/[^/]+\/run$/.test(response.url()));
  await click(row.getByRole("button", { name: /Run|运行/, exact: true }));
  const runResponse = await responsePromise;
  expect(runResponse.ok()).toBeTruthy();
  const deploymentRun = await runResponse.json();
  await checkpoint("the trigger produces a visible deployment-run receipt", async () => {
    await expect(page.getByText(deploymentRun.id, { exact: true })).toBeVisible();
    const response = await page.request.get(`http://127.0.0.1:38080/v1/deployment_runs?deployment_id=${deploymentRun.deployment_id}`);
    const body = await response.json();
    expect(body.data).toEqual(expect.arrayContaining([expect.objectContaining({ id: deploymentRun.id })]));
  });
  await clearCaption();
  await aha(story.aha);
  await wait(900);
  await clearCaption();
}
