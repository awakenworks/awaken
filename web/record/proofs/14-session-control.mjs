// Session-control proof: interrupt is acknowledged and archive becomes a hard boundary.
import { configureSyntheticModel } from "../support/models.mjs";
import { MANAGED_HEADERS } from "../support/betas.mjs";
import { BACKEND, createManagedSession, publishAgent, putAgent, requireOk } from "../support/control-plane.mjs";

const AGENT_ID = "controlled-session-agent";
const MODEL_ID = "session-control-recording-model";

export async function run({ page, goto, intro, say, clearCaption, checkpoint, aha, expect, click, type, wait }) {
  await configureSyntheticModel(page, MODEL_ID);
  await putAgent(page, AGENT_ID, {
      id: AGENT_ID, name: "Controlled session agent", model: { id: MODEL_ID },
      system: "Remain controllable.", tools: [], mcp_servers: [], skills: [],
      plugins: [], plugin_config: {}, context_policy: { kind: "keep_all" }, max_steps: 4,
  });
  await publishAgent(page, AGENT_ID);
  const session = await createManagedSession(
    page,
    { agent: AGENT_ID, title: "Operator-controlled session" },
    MANAGED_HEADERS,
  );

  await goto(`/w/default/sessions/${session.id}`);
  await intro(
    "A running Agent must stop without deleting the evidence already recorded.",
    "Interrupt the run, archive the Session, then prove a late message cannot restart the work.",
  );
  await say("The operator starts the work, then sends a real interrupt.", 3200);
  const composer = page.getByPlaceholder(/Message|输入消息/);
  await expect(composer).toBeEnabled({ timeout: 60_000 });
  await type(composer, "Continue until the operator stops this run.", { delay: 18 });
  await click(page.getByRole("button", { name: /Send|发送/, exact: true }));
  await expect.poll(async () => {
    const read = await page.request.get(`${BACKEND}/v1/sessions/${session.id}/events`, { headers: MANAGED_HEADERS });
    await requireOk(read, "admitted Session message");
    return (await read.json()).data.some((event) => event.type === "user.message");
  }, { timeout: 30_000 }).toBeTruthy();
  await expect(page.getByRole("button", { name: /Stop run|停止运行/i, exact: true })).toBeEnabled();
  await click(page.getByRole("button", { name: /Stop run|停止运行/i, exact: true }));
  await click(page.getByRole("alertdialog").getByRole("button", { name: /Stop run|停止运行/i, exact: true }));
  await expect(page.getByText(/user\.interrupt accepted · evt_/)).toBeVisible({ timeout: 15_000 });
  await checkpoint("the operator receives a visible interrupt receipt", async () => {
    const receipt = await page.request.get(`${BACKEND}/v1/sessions/${session.id}/events`, { headers: MANAGED_HEADERS });
    await requireOk(receipt, "Session interrupt receipt readback");
    const body = await receipt.json();
    expect(body.data.some((event) => event.type === "user.interrupt")).toBeTruthy();
    await expect(page.getByText(/user\.interrupt accepted · evt_/)).toBeVisible();
  });

  await say("Archiving turns the stopped Session into a fixed record.", 3400);
  await click(page.getByRole("button", { name: /archive|归档/i, exact: true }));
  await click(page.getByRole("alertdialog").getByRole("button", { name: /archive|归档/i, exact: true }));
  await checkpoint("archive is visible and the managed Session remains retrievable", async () => {
    await expect(page.locator(".pill").filter({ hasText: /^archived$|^已归档$/ })).toBeVisible();
    const read = await page.request.get(`${BACKEND}/v1/sessions/${session.id}`, { headers: MANAGED_HEADERS });
    await requireOk(read, "archived Session readback");
    const body = await read.json();
    expect(body.archived_at).toBeTruthy();
    const rejected = await page.request.post(`${BACKEND}/v1/sessions/${session.id}/events`, {
      headers: MANAGED_HEADERS,
      data: { events: [{ type: "user.message", content: [{ type: "text", text: "new work" }] }] },
    });
    expect(rejected.status()).toBe(409);
  });
  await clearCaption();
  await wait(900);
  await clearCaption();
}
