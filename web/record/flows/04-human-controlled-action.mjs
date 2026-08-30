// Review a real source file from the current checkout, stop at the write
// permission boundary, then let a person approve one downloadable artifact.

import { createHash } from "node:crypto";
import { execFileSync } from "node:child_process";
import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import { setTimeout as delay } from "node:timers/promises";
import { configureSyntheticModel } from "../support/models.mjs";
import { FILES_HEADERS, MANAGED_HEADERS } from "../support/betas.mjs";
import {
  BACKEND,
  createManagedSession,
  publishAgent,
  putAgent,
  putAgentResources,
  requireOk,
  sendManagedEvents,
  uploadAgentFile,
} from "../support/control-plane.mjs";

const REPOSITORY = resolve(import.meta.dirname, "../../..");
const SOURCE_RELATIVE = "web/src/surfaces/protocols.tsx";
const SOURCE_FILE = resolve(REPOSITORY, SOURCE_RELATIVE);
const SOURCE_MOUNT = "/mnt/files/protocols.tsx";
const TOOL_SOURCE = `/mnt/session/uploads${SOURCE_MOUNT}`;
const ARTIFACT_NAME = "protocol-onboarding-review.md";
const ARTIFACT_PATH = `/mnt/session/outputs/${ARTIFACT_NAME}`;
const AGENT_ID = "repository-change-reviewer";
const MODEL_ID = "recording-repository-action-model";

export const story = {
  job: "Verify protocol onboarding in a real checkout",
  stakes: "A useful source review must cite the real revision, while producing a new artifact remains a human-controlled action.",
  handoff: "The Session retains the checkout revision, read trace, approval receipt, and downloadable verification.",
  promise: "Let an Agent investigate real source code without giving it silent write authority.",
  effect: "The Agent reads one pinned source snapshot, waits for approval, and produces one inspectable review artifact.",
  aha: "The source stays read-only. Awaken writes the reviewed artifact only after approval.",
  loyalty: "The same permission boundary can govern every repository review without hiding what was approved.",
  satisfaction: "The work ends in a downloadable verification tied to its source revision and approval.",
  advocacy: "A reviewer can verify both the conclusion and the exact source that produced it.",
};

let prepared;

function sha256(text) {
  return createHash("sha256").update(text).digest("hex");
}

async function sessionEvents(page, sessionId) {
  const response = await page.request.get(`${BACKEND}/v1/sessions/${sessionId}/events`, { headers: MANAGED_HEADERS });
  await requireOk(response, "repository review Event readback");
  return (await response.json()).data;
}

export async function prepare({ page }) {
  await configureSyntheticModel(page, MODEL_ID);
  const source = readFileSync(SOURCE_FILE, "utf8");
  if (!source.includes('@anthropic-ai/sdk')
    || !source.includes("Create or manage service API keys")
    || !source.includes("awakenworks.com")
    || !source.includes("CopyButton")) {
    throw new Error("the real protocol source no longer matches this review's verified premise");
  }
  const revision = execFileSync("git", ["rev-parse", "--short=12", "HEAD"], {
    cwd: REPOSITORY,
    encoding: "utf8",
    timeout: 10_000,
  }).trim();
  const sourceSha256 = sha256(source);
  const file = await uploadAgentFile(page, { name: "protocols.tsx", content: source, mimeType: "text/typescript" });

  await putAgent(page, AGENT_ID, {
    id: AGENT_ID,
    name: "Repository change reviewer",
    description: "Reads a pinned source snapshot and prepares a verification artifact behind an explicit write approval.",
    model: { id: MODEL_ID },
    system: `First call read exactly once on ${TOOL_SOURCE}. Confirm from the source that it contains an @anthropic-ai/sdk example, a service-key management link, an awakenworks.com protocol documentation link, and a CopyButton for the code example. If any premise is false, stop without writing. Otherwise call write exactly once at ${ARTIFACT_PATH}. The file must use these headings: Protocol onboarding verification, Source checked, Verified path, Copy affordance, Approval boundary, Result. Include repository revision ${revision}, source path ${SOURCE_RELATIVE}, source SHA-256 ${sourceSha256}, and the exact filename ${ARTIFACT_NAME}. State that the review artifact was written only after approval and the mounted source remained read-only. After write succeeds, reply with exactly these headings: Repository checked, Approval, Artifact. Never modify the mounted source.`,
    tools: [{
      type: "agent_toolset_20260401",
      configs: [
        { name: "read", type: "read", enabled: true, permission_policy: { type: "always_allow" } },
        { name: "write", type: "write", enabled: true, permission_policy: { type: "always_ask" } },
      ],
      default_config: { enabled: false, permission_policy: { type: "always_ask" } },
    }],
    tool_overrides: [],
    mcp_servers: [],
    skills: [],
    max_steps: 7,
    plugins: ["compact"],
    plugin_config: {
      compact: {},
    },
    context_policy: { kind: "keep_last", keep_last: 18 },
  });
  await putAgentResources(page, AGENT_ID, [
    { binding_id: "protocol-source", target: { kind: "file", id: file.id }, mount_path: SOURCE_MOUNT, access: "read_only" },
  ]);
  await publishAgent(page, AGENT_ID);
  const session = await createManagedSession(page, {
    agent: AGENT_ID,
    title: "Review protocol onboarding source",
    metadata: { repository_revision: revision, source_sha256: sourceSha256, source_path: SOURCE_RELATIVE },
  }, MANAGED_HEADERS);
  await sendManagedEvents(page, session.id, [{
    type: "user.message",
    content: [{ type: "text", text: "Review the mounted protocol source, then call write exactly once now. Awaken will pause that tool call before execution so a person can review it. Do not approve or bypass the pending write yourself." }],
  }], MANAGED_HEADERS);

  const deadline = Date.now() + 420_000;
  while (Date.now() < deadline) {
    const events = await sessionEvents(page, session.id);
    const failure = events.find((event) => event.type === "session.error");
    if (failure) throw new Error(`repository review failed before approval: ${JSON.stringify(failure)}`);
    const read = events.find((event) => event.type === "agent.tool_use" && event.name === "read");
    const readResult = read && events.find((event) => event.type === "agent.tool_result" && event.tool_use_id === read.id);
    const write = events.find((event) => event.type === "agent.tool_use" && event.name === "write"
      && event.evaluated_permission === "ask" && event.input?.file_path === ARTIFACT_PATH);
    const awaiting = events.findLast((event) => event.type === "session.status_idle")?.stop_reason?.type === "requires_action";
    const prematureWrite = write && events.some((event) => event.type === "agent.tool_result" && event.tool_use_id === write.id);
    if (prematureWrite) throw new Error("the protected artifact write executed before human approval");
    if (readResult && write && awaiting) {
      prepared = { file, session, revision, sourceSha256, read, write };
      return;
    }
    await delay(1_000);
  }
  throw new Error("repository review did not reach its write approval within 420000ms");
}

export async function run({ page, goto, intro, beat, say, clearCaption, checkpoint, runtimeCheckpoint, aha, expect, click, wait }) {
  if (!prepared) await prepare({ page });
  const { file, session, revision, sourceSha256, read, write } = prepared;

  await goto(`/w/default/sessions/${session.id}`);
  await intro(
    "Review this checkout's real protocol onboarding code, but do not let the Agent write silently.",
    "Awaken pins the source, records the read, and pauses the proposed artifact for approval.",
  );
  await say("Deterministic response; the read, approval, write, and download are real.", 3800);

  await click(page.getByRole("button", { name: /Inputs|输入/, exact: true }));
  const sourceInput = page.getByText(SOURCE_MOUNT, { exact: true });
  await checkpoint("the Session receives the exact source snapshot as read-only input", async () => {
    await expect(sourceInput).toBeVisible({ timeout: 15_000 });
    await expect(page.getByText(/Read only|只读/, { exact: true })).toBeVisible();
    const resources = await page.request.get(`${BACKEND}/v1/sessions/${session.id}/resources`, { headers: MANAGED_HEADERS });
    await requireOk(resources, "repository source Resource readback");
    expect((await resources.json()).data).toEqual(expect.arrayContaining([
      expect.objectContaining({ file_id: file.id, mount_path: SOURCE_MOUNT }),
    ]));
  });
  await beat(`Pinned to ${revision}. The source hash travels with this Session.`, sourceInput, 2800);

  await click(page.getByRole("button", { name: /Chat|对话/, exact: true }));
  const readCard = page.getByRole("region", { name: /Tool read|工具 read/, exact: true });
  await checkpoint("the Agent reads the pinned source before preparing its verification", async () => {
    await expect(readCard).toBeVisible({ timeout: 15_000 });
    const events = await sessionEvents(page, session.id);
    expect(events.some((event) => event.type === "agent.tool_result" && event.tool_use_id === read.id)).toBeTruthy();
    const sessionRead = await page.request.get(`${BACKEND}/v1/sessions/${session.id}`, { headers: MANAGED_HEADERS });
    await requireOk(sessionRead, "repository review Session metadata readback");
    expect((await sessionRead.json()).metadata).toMatchObject({ repository_revision: revision, source_sha256: sourceSha256 });
  });
  await beat("The trace records the pinned source and revision before the Agent proposes any output.", readCard, 3000);

  const writeCard = page.getByRole("region", { name: /Tool write|工具 write/, exact: true });
  await checkpoint("the verification artifact is blocked at an explicit write approval", async () => {
    await expect(writeCard).toBeVisible({ timeout: 15_000 });
    await expect(writeCard).toContainText(/awaiting approval|待确认/i);
    await expect(writeCard).toContainText(ARTIFACT_PATH);
    const events = await sessionEvents(page, session.id);
    expect(events.some((event) => event.id === write.id && event.evaluated_permission === "ask")).toBeTruthy();
    expect(events.some((event) => event.type === "agent.tool_result" && event.tool_use_id === write.id)).toBeFalsy();
  });
  await beat("The Agent proposes one file and stops. Its path and content are visible before approval.", writeCard, 3400);

  await say("Approve this one artifact after reviewing its path and content.", 2600);
  await click(writeCard.getByRole("button", { name: /Allow|允许/, exact: true }));

  let artifact;
  const answer = page.locator('[data-role="assistant"]').last();
  await runtimeCheckpoint("approval executes one write and commits the final handoff", async () => {
    await expect(answer).toContainText(/Repository checked[\s\S]*Approval[\s\S]*Artifact/i, { timeout: 300_000 });
    await expect.poll(async () => {
      const response = await page.request.get(`${BACKEND}/v1/files?scope_id=${session.id}`, { headers: FILES_HEADERS });
      await requireOk(response, "repository review Artifact listing");
      artifact = (await response.json()).data.find((candidate) => candidate.filename?.includes(ARTIFACT_NAME));
      return Boolean(artifact);
    }, { timeout: 120_000 }).toBeTruthy();
    const events = await sessionEvents(page, session.id);
    expect(events.filter((event) => event.type === "user.tool_confirmation" && event.tool_use_id === write.id
      && event.result === "allow")).toHaveLength(1);
    expect(events.filter((event) => event.type === "agent.tool_result" && event.tool_use_id === write.id)).toHaveLength(1);
  });
  await beat("This approval executes the proposed write once and records the result in the Session.", answer, 3400);

  await click(page.locator(".session-detail-layout .segmented").getByRole("button", { name: /Artifacts|产物/, exact: true }));
  const artifactRow = page.getByText(new RegExp(ARTIFACT_NAME.replace(".", "\\.")));
  await checkpoint("the approved review is downloadable and tied to the real checkout", async () => {
    await expect(artifactRow).toBeVisible({ timeout: 15_000 });
    const content = await page.request.get(`${BACKEND}/v1/files/${artifact.id}/content`, { headers: FILES_HEADERS });
    await requireOk(content, "repository review Artifact download");
    const text = await content.text();
    expect(text).toMatch(/Protocol onboarding verification[\s\S]*Source checked[\s\S]*Verified path[\s\S]*Copy affordance[\s\S]*Approval boundary[\s\S]*Result/i);
    expect(text).toContain(revision);
    expect(text).toContain(SOURCE_RELATIVE);
    expect(text).toContain(sourceSha256);
    expect(text).toMatch(/CopyButton|copy example/i);
  });
  await beat("The deliverable stays beside its source, trace, and approval receipt.", artifactRow, 3200);

  await clearCaption();
  await aha(story.aha, 5200);
  await wait(800);
  await clearCaption();
}
