// Session-control proof: interrupt is acknowledged and archive becomes a hard boundary.
import { configureSyntheticModel } from "../support/models.mjs";

const AGENT_ID = "controlled-session-agent";
const MODEL_ID = "session-control-recording-model";

export const story = {
  promise: "Interrupt one Session, archive it without deleting evidence, then prove the lifecycle boundary rejects new work.",
  effect: "Interrupt returns a visible receipt; archive remains visible and makes every later event write fail closed.",
  aha: "Control does not erase evidence—the archived Session remains inspectable while refusing every new write.",
  loyalty: "Recoverable operator control builds confidence to entrust longer and more valuable work to the platform.",
  satisfaction: "An immediate receipt and explicit archived state make lifecycle management predictable instead of mysterious.",
  advocacy: "The visible shift from accepted control to fail-closed archive is concise governance proof for operations teams.",
};

export async function run({ page, goto, intro, say, clearCaption, checkpoint, aha, expect, click, wait }) {
  await configureSyntheticModel(page, MODEL_ID);
  await page.request.put(`http://127.0.0.1:38080/v1/config/agents/${AGENT_ID}`, {
    data: {
      id: AGENT_ID, name: "Controlled session agent", model: { id: MODEL_ID },
      system: "Remain controllable.", tools: [], mcp_servers: [], skills: [],
      plugins: [], plugin_config: {}, context_policy: { kind: "keep_all" }, max_steps: 4,
    },
  });
  const published = await page.request.post(`http://127.0.0.1:38080/v1/config/agents/${AGENT_ID}/publish`);
  expect(published.ok()).toBeTruthy();
  const response = await page.request.post("http://127.0.0.1:38080/v1/sessions", {
    data: { agent: AGENT_ID, title: "Operator-controlled session" },
  });
  const session = await response.json();

  await goto(`/w/default/sessions/${session.id}`);
  await intro(
    "Keep human authority over Agent work after a Session has already been created.",
    "Acknowledge interrupt through the managed event endpoint, then archive the Session into an enforced read-only state.",
  );
  await say("Interrupt reaches the managed lifecycle without deleting the Session or hiding whether the request was accepted.", 4200);
  const interruptResponse = page.waitForResponse((res) =>
    res.url().endsWith(`/v1/sessions/${session.id}/events`) && res.request().method() === "POST");
  await click(page.getByRole("button", { name: /interrupt/i, exact: true }));
  const interrupt = await interruptResponse;
  await checkpoint("the operator receives a visible interrupt receipt", async () => {
    expect(interrupt.ok()).toBeTruthy();
    const body = await interrupt.json();
    expect(body.data[0].type).toBe("user.interrupt");
    await expect(page.getByText(/user\.interrupt accepted · evt_/)).toBeVisible();
  });

  await say("Archive closes the operational lifecycle without erasing the Session record.", 3400);
  await click(page.getByRole("button", { name: /archive|归档/i, exact: true }));
  await checkpoint("archive is visible and the managed Session remains retrievable", async () => {
    await expect(page.getByText(/archived|已归档/, { exact: true })).toBeVisible();
    const read = await page.request.get(`http://127.0.0.1:38080/v1/sessions/${session.id}`);
    const body = await read.json();
    expect(body.archived_at).toBeTruthy();
    const rejected = await page.request.post(`http://127.0.0.1:38080/v1/sessions/${session.id}/events`, {
      data: { events: [{ type: "user.message", content: [{ type: "text", text: "new work" }] }] },
    });
    expect(rejected.status()).toBe(409);
  });
  await clearCaption();
  await aha(story.aha);
  await wait(900);
  await clearCaption();
}
