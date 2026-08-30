// Begin one gated work item, restart the release all-in-one process over the
// same data directory, then approve and finish the exact same Session.

import { setTimeout as delay } from "node:timers/promises";
import { configureSyntheticModel } from "../support/models.mjs";
import { FILES_HEADERS, MANAGED_HEADERS } from "../support/betas.mjs";
import {
  BACKEND,
  createManagedSession,
  publishAgent,
  putAgent,
  requireOk,
  sendManagedEvents,
} from "../support/control-plane.mjs";

const AGENT_ID = "restart-continuity-agent";
const MARKER = "RESTART-CONTINUITY-41";
const ARTIFACT_NAME = "restart-continuity.md";
const ARTIFACT_PATH = `/mnt/session/outputs/${ARTIFACT_NAME}`;
const MODEL_ID = "recording-restart-continuity-model";

export const story = {
  job: "Continue accepted Agent work after service restart",
  stakes: "A service restart must not erase the Session, its accepted input, or a pending human decision.",
  handoff: "The same Session survives a new all-in-one process incarnation, accepts its original approval, and produces the requested artifact.",
  promise: "Restart Awaken without rebuilding accepted Agent work from chat logs or memory.",
  effect: "A pending write and its human boundary reappear on the same Session before the work completes once.",
  aha: "Awaken restarts, reopens the same pending decision, and finishes the original Session.",
  loyalty: "Maintenance and recovery no longer force operators to reconstruct the last accepted task.",
  satisfaction: "The viewer sees one identity move from pending to complete across a real restart.",
  advocacy: "A team can demonstrate recovery with the same Session id, receipt, approval, and artifact.",
};

let prepared;

async function readEvents(page, sessionId) {
  const response = await page.request.get(`${BACKEND}/v1/sessions/${sessionId}/events`, { headers: MANAGED_HEADERS });
  await requireOk(response, "restart continuity Event readback");
  return (await response.json()).data;
}

export async function prepare({ page }) {
  await configureSyntheticModel(page, MODEL_ID);
  await putAgent(page, AGENT_ID, {
    id: AGENT_ID,
    name: "Restart continuity agent",
    description: "Completes one accepted work item while preserving its pending approval across service recovery.",
    model: { id: MODEL_ID },
    system: `Call write exactly once at ${ARTIFACT_PATH}. Its content must use the headings Work item, Accepted marker, Durable handoff and include ${MARKER}. Then reply with exactly these headings: Work resumed, Artifact, Marker. Do not call another tool.`,
    tools: [{
      type: "agent_toolset_20260401",
      configs: [
        { name: "write", type: "write", enabled: true, permission_policy: { type: "always_ask" } },
      ],
      default_config: { enabled: false, permission_policy: { type: "always_ask" } },
    }],
    tool_overrides: [],
    mcp_servers: [],
    skills: [],
    max_steps: 5,
    plugins: ["compact"],
    plugin_config: {
      compact: {},
    },
    context_policy: { kind: "keep_last", keep_last: 12 },
  });
  await publishAgent(page, AGENT_ID);
  const session = await createManagedSession(page, {
    agent: AGENT_ID,
    title: "Continue work across restart",
    metadata: { continuity_marker: MARKER, recording_case: "real-process-restart" },
  }, MANAGED_HEADERS);
  const receipt = await sendManagedEvents(page, session.id, [{
    type: "user.message",
    content: [{ type: "text", text: `Call write exactly once now to create the continuity artifact for ${MARKER}. Awaken will pause that tool call before execution so a person can review it. Do not approve or bypass the pending write yourself.` }],
  }], MANAGED_HEADERS);
  const acceptedEventId = receipt.data[0]?.id;
  if (typeof acceptedEventId !== "string") throw new Error("restart story did not receive an accepted User Event id");

  const deadline = Date.now() + 420_000;
  while (Date.now() < deadline) {
    const events = await readEvents(page, session.id);
    const failure = events.find((event) => event.type === "session.error");
    if (failure) throw new Error(`restart story failed before process restart: ${JSON.stringify(failure)}`);
    const write = events.find((event) => event.type === "agent.tool_use" && event.name === "write"
      && event.evaluated_permission === "ask" && event.input?.file_path === ARTIFACT_PATH);
    const latestIdle = events.findLast((event) => event.type === "session.status_idle");
    if (write && latestIdle?.stop_reason?.type === "requires_action") {
      if (events.some((event) => event.type === "agent.tool_result" && event.tool_use_id === write.id)) {
        throw new Error("restart story write executed before its approval and restart boundary");
      }
      prepared = { session, write, acceptedEventId };
      return;
    }
    await delay(1_000);
  }
  throw new Error("restart story did not reach its durable approval boundary within 420000ms");
}

export async function run({ page, goto, intro, beat, say, clearCaption, checkpoint, runtimeCheckpoint, aha, expect, click, wait, restartAllInOne }) {
  if (!prepared) await prepare({ page });
  const { session, write, acceptedEventId } = prepared;
  const sessionPath = `/w/default/sessions/${session.id}`;

  await goto(sessionPath);
  await intro(
    "A service restart must not erase accepted work or a pending human decision.",
    "Restart the release all-in-one process, reopen the same Session, and continue from its approval boundary.",
  );
  await say("Deterministic response; the restart, recovery, approval, and artifact are live.", 3600);

  let writeCard = page.getByRole("region", { name: /Tool write|工具 write/, exact: true });
  let technicalId = page.locator("details.technical-id").filter({ hasText: session.id });
  await click(technicalId.getByText(/Technical ID|技术 ID/, { exact: true }));
  await checkpoint("the accepted work is durably waiting at its human boundary", async () => {
    await expect(technicalId.getByText(session.id, { exact: true })).toBeVisible({ timeout: 15_000 });
    await expect(writeCard).toContainText(/awaiting approval|待确认/i);
    await expect(writeCard).toContainText(ARTIFACT_PATH);
    const events = await readEvents(page, session.id);
    expect(events.some((event) => event.id === acceptedEventId && event.type === "user.message")).toBeTruthy();
    expect(events.some((event) => event.id === write.id && event.evaluated_permission === "ask")).toBeTruthy();
    expect(events.some((event) => event.type === "agent.tool_result" && event.tool_use_id === write.id)).toBeFalsy();
  });
  await beat("Before restart, Awaken has committed both the accepted request and its pending write.", writeCard, 3400);

  await say("Now replace the running process, while keeping its persistent data root.", 3000);
  let restartReceipt;
  await checkpoint("a new release all-in-one process restores the same pending Session", async () => {
    restartReceipt = await restartAllInOne();
    expect(restartReceipt.after_pid).not.toBe(restartReceipt.before_pid);
    expect(restartReceipt.generation).toBe(2);
    await page.goto(`${BACKEND}${sessionPath}`, { waitUntil: "domcontentloaded", timeout: 30_000 });
    await expect(page.locator("main")).toBeVisible({ timeout: 20_000 });
    technicalId = page.locator("details.technical-id").filter({ hasText: session.id });
    await click(technicalId.getByText(/Technical ID|技术 ID/, { exact: true }));
    await expect(technicalId.getByText(session.id, { exact: true })).toBeVisible({ timeout: 20_000 });
    writeCard = page.getByRole("region", { name: /Tool write|工具 write/, exact: true });
    await expect(writeCard).toContainText(/awaiting approval|待确认/i, { timeout: 20_000 });
    const read = await page.request.get(`${BACKEND}/v1/sessions/${session.id}`, { headers: MANAGED_HEADERS });
    await requireOk(read, "post-restart Session readback");
    expect((await read.json()).metadata).toMatchObject({ continuity_marker: MARKER, recording_case: "real-process-restart" });
    const events = await readEvents(page, session.id);
    expect(events.some((event) => event.id === acceptedEventId)).toBeTruthy();
    expect(events.some((event) => event.id === write.id && event.evaluated_permission === "ask")).toBeTruthy();
    expect(events.some((event) => event.type === "agent.tool_result" && event.tool_use_id === write.id)).toBeFalsy();
  });
  await beat("The new process restores the same Session ID, request, and pending approval.", writeCard, 3400);

  await say("Approve the original request. Nothing is recreated or resubmitted.", 2600);
  await click(writeCard.getByRole("button", { name: /Allow|允许/, exact: true }));
  let artifact;
  const answer = page.locator('[data-role="assistant"]').last();
  await runtimeCheckpoint("the recovered work completes once after its original approval", async () => {
    await expect(answer).toContainText(/Work resumed[\s\S]*Artifact[\s\S]*Marker[\s\S]*RESTART-CONTINUITY-41/i, { timeout: 300_000 });
    await expect.poll(async () => {
      const response = await page.request.get(`${BACKEND}/v1/files?scope_id=${session.id}`, { headers: FILES_HEADERS });
      await requireOk(response, "restart continuity Artifact listing");
      artifact = (await response.json()).data.find((candidate) => candidate.filename?.includes(ARTIFACT_NAME));
      return Boolean(artifact);
    }, { timeout: 120_000 }).toBeTruthy();
    const events = await readEvents(page, session.id);
    expect(events.filter((event) => event.type === "user.tool_confirmation" && event.tool_use_id === write.id
      && event.result === "allow")).toHaveLength(1);
    expect(events.filter((event) => event.type === "agent.tool_result" && event.tool_use_id === write.id)).toHaveLength(1);
  });
  await beat("The recovered run finishes once and keeps the original marker intact.", answer, 3200);

  await click(page.locator(".session-detail-layout .segmented").getByRole("button", { name: /Artifacts|产物/, exact: true }));
  const artifactRow = page.getByText(new RegExp(ARTIFACT_NAME.replace(".", "\\.")));
  await checkpoint("the post-restart artifact remains attached to the original Session", async () => {
    await expect(artifactRow).toBeVisible({ timeout: 15_000 });
    const response = await page.request.get(`${BACKEND}/v1/files/${artifact.id}/content`, { headers: FILES_HEADERS });
    await requireOk(response, "restart continuity Artifact download");
    expect(await response.text()).toMatch(/Work item[\s\S]*Accepted marker[\s\S]*Durable handoff[\s\S]*RESTART-CONTINUITY-41/i);
  });
  await beat("One Session now holds the pre-restart request and post-restart result.", artifactRow, 3200);

  await clearCaption();
  await aha(story.aha, 5200);
  await wait(800);
  await clearCaption();
}
