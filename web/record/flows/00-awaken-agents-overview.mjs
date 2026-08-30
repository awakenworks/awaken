// The series opener: a governed Agent reads one mounted evidence packet and
// produces a decision brief with an explicit human approval boundary.

import { setTimeout as delay } from "node:timers/promises";
import { typedAgentTools } from "../../test-support/agent-tools.mjs";
import { LIVE_MODEL_ID, configureLiveModel } from "../support/models.mjs";
import { MANAGED_HEADERS } from "../support/betas.mjs";
import {
  BACKEND,
  createManagedSession,
  publishAgent,
  putAgent,
  putAgentResources,
  requireOk,
  uploadAgentFile,
} from "../support/control-plane.mjs";

const AGENT = "release-evidence-reviewer";
const EVIDENCE_PATH = "/mnt/files/release-evidence.txt";
const TOOL_PATH = `/mnt/session/uploads${EVIDENCE_PATH}`;
const EVIDENCE = `CONTROLLED RELEASE EVIDENCE

VERIFIED CHECKS
- Console regression: PASS 412/412.
- Export regression: FAIL INV-204.
- Database migration rehearsal: NOT RUN. Owner: UNASSIGNED.
- Rollback runbook: STALE.

CONTROL POLICY
- Production deployment requires human approval.
- A database migration cannot proceed without a successful rehearsal, a named owner, and current rollback evidence.
- The reviewer may recommend and assign follow-up work. It may not deploy, approve, or invent missing evidence.`;

export const story = {
  job: "Release evidence review",
  stakes: "A failed regression, an unowned migration rehearsal, and stale rollback evidence must prevent an unsupported release decision.",
  handoff: "The result is a HOLD brief tied to mounted evidence, with the failed check, stale runbook, and missing owner visible for review.",
  promise: "Give Awaken a release evidence packet and receive a decision a person can audit and act on.",
  effect: "The Agent reads the mounted file, separates passed checks from blockers, and preserves the human approval boundary.",
  aha: "Awaken holds the release because the evidence shows exactly what must be fixed first.",
  loyalty: "Every release can use the same governed review without rebuilding evidence from chat fragments.",
  satisfaction: "The source, Agent read trace, and decision remain together in one durable Session.",
  advocacy: "The team can challenge the evidence behind the decision, not guess how the Agent reached it.",
};

let prepared;

export async function prepare({ page }) {
  await configureLiveModel(page);
  const file = await uploadAgentFile(page, {
    name: "release-evidence.txt",
    content: EVIDENCE,
  });
  await putAgent(page, AGENT, {
    id: AGENT,
    name: "Release evidence reviewer",
    description: "Reads the specified read-only release material and prepares a reviewable decision.",
    model: { id: LIVE_MODEL_ID },
    system: `First call read exactly once on ${TOOL_PATH}. Use only that file as evidence. Produce a concise brief with exactly these headings: Decision, Evidence reviewed, Blockers, Owners, Approval required, Next actions. The decision must be HOLD when any required check is failed, missing, unowned, or stale. Never deploy, approve, or invent evidence. Stay under 180 words.`,
    // Evidence decision rule: C1 a mounted read-only packet requires `read`;
    // E1 exactly one typed Agent ToolSet enables it without a legacy scalar path.
    tools: typedAgentTools(["read"]),
    tool_overrides: [],
    mcp_servers: [],
    skills: [],
    max_steps: 6,
    plugins: ["compact", "state_machine"],
    plugin_config: { compact: {}, state_machine: { machines: [] } },
    context_policy: { kind: "keep_last", keep_last: 24 },
  });
  await putAgentResources(page, AGENT, [
    { binding_id: "release-evidence", target: { kind: "file", id: file.id }, mount_path: EVIDENCE_PATH, access: "read_only" },
  ]);
  await publishAgent(page, AGENT);
  const session = await createManagedSession(page, {
    agent: AGENT,
    title: "Controlled release evidence review",
  }, MANAGED_HEADERS);
  const dispatch = await page.request.post(`${BACKEND}/v1/sessions/${session.id}/events`, {
    headers: MANAGED_HEADERS,
    data: { events: [{ type: "user.message", content: [{ type: "text", text: "Review the mounted release evidence and prepare a decision brief for human review." }] }] },
  });
  await requireOk(dispatch, "release evidence review dispatch");

  const deadline = Date.now() + 420_000;
  while (Date.now() < deadline) {
    const response = await page.request.get(`${BACKEND}/v1/sessions/${session.id}/events`, { headers: MANAGED_HEADERS });
    await requireOk(response, "release evidence result readback");
    const events = await response.json();
    const error = events.data.find((event) => event.type === "session.error");
    if (error) throw new Error(`release evidence review failed: ${JSON.stringify(error)}`);
    const read = events.data.some((event) => event.type === "agent.tool_use" && event.name === "read");
    const decision = events.data.some((event) => {
      if (event.type !== "agent.message") return false;
      const text = (event.content ?? []).map((content) => content.text ?? "").join("\n");
      return /Decision[\s\S]*HOLD/i.test(text)
        && /INV-204/.test(text)
        && /UNASSIGNED/i.test(text)
        && /STALE/i.test(text);
    });
    if (read && decision) {
      prepared = { file, session };
      return;
    }
    await delay(1_000);
  }
  throw new Error("release evidence review did not complete within 420000ms");
}

export async function run({ page, goto, intro, beat, say, clearCaption, checkpoint, aha, expect, click, wait }) {
  if (!prepared) await prepare({ page });
  const { file, session } = prepared;

  await goto(`/w/default/sessions/${session.id}`);
  await intro(
    "This release cannot proceed: one check failed, the rollback guide is stale, and nobody owns the migration rehearsal.",
    "Awaken reads the evidence, explains the HOLD, and leaves approval with a person.",
  );
  await say("Controlled test data. This live run reads, traces, and decides.", 3400);

  await click(page.getByRole("button", { name: /Inputs|输入/, exact: true }));
  const mountedFile = page.getByText(EVIDENCE_PATH, { exact: true });
  const readOnly = page.getByText(/Read only|只读/, { exact: true });
  await checkpoint("the Session receives the exact evidence file as a read-only input", async () => {
    await expect(mountedFile).toBeVisible({ timeout: 15_000 });
    await expect(readOnly).toBeVisible({ timeout: 15_000 });
    const response = await page.request.get(`${BACKEND}/v1/sessions/${session.id}/resources`, { headers: MANAGED_HEADERS });
    await requireOk(response, "Session evidence provenance readback");
    const resources = await response.json();
    expect(resources.data).toEqual(expect.arrayContaining([
      expect.objectContaining({ file_id: file.id, mount_path: EVIDENCE_PATH }),
    ]));
    const authoredResponse = await page.request.get(`${BACKEND}/v1/config/agents/${AGENT}/resources`);
    await requireOk(authoredResponse, "Agent evidence access readback");
    const authored = await authoredResponse.json();
    expect(authored.inputs).toEqual(expect.arrayContaining([
      expect.objectContaining({
        target: { kind: "file", id: file.id },
        mount_path: EVIDENCE_PATH,
        access: "read_only",
      }),
    ]));
  });
  await beat("This is the evidence the Agent receives. It is mounted read-only and stays attached to the Session.", mountedFile, 3200);

  await click(page.getByRole("button", { name: /Chat|对话/, exact: true }));
  const readCard = page.getByRole("region", { name: /Tool read|工具 read/, exact: true }).first();
  await checkpoint("the real Agent reads the mounted packet before deciding", async () => {
    await expect(readCard).toBeVisible({ timeout: 15_000 });
    const response = await page.request.get(`${BACKEND}/v1/sessions/${session.id}/events`, { headers: MANAGED_HEADERS });
    await requireOk(response, "release evidence tool receipt readback");
    const events = await response.json();
    expect(events.data.some((event) => event.type === "agent.tool_use" && event.name === "read")).toBeTruthy();
  });
  await beat("Before the decision appears, the trace records the exact file the Agent read.", readCard, 3000);

  const report = page.locator('[data-role="assistant"]').last();
  await checkpoint("the evidence produces a complete HOLD brief with owners and approval boundary", async () => {
    await expect(report).toContainText(/Decision[\s\S]*HOLD/i, { timeout: 15_000 });
    await expect(report).toContainText(/INV-204/);
    await expect(report).toContainText(/UNASSIGNED/i);
    await expect(report).toContainText(/STALE/i);
    await expect(report).toContainText(/Approval required/i);
  });
  await beat("The HOLD names the failed check, stale runbook, and unowned rehearsal.", report, 3400);
  await say("The brief names next steps but cannot approve or deploy.", 2800);

  await goto(`/w/default/sessions/${session.id}`);
  await checkpoint("the decision and its source survive a fresh navigation", async () => {
    await expect(page.locator('[data-role="assistant"]').last()).toContainText(/HOLD[\s\S]*INV-204/i, { timeout: 15_000 });
  });
  await beat("Reopen the Session and the evidence, trace, decision, and approval boundary are still together.", page.getByText("Controlled release evidence review", { exact: true }), 3400);

  await clearCaption();
  await aha(story.aha, 5200);
  await wait(900);
  await clearCaption();
}
