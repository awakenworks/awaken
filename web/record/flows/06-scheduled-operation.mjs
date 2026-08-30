// Convert a live repository snapshot into repeatable, exception-only maintenance
// work. Each manual or scheduled firing owns a separate inspectable Session.
import { execFileSync } from "node:child_process";
import { basename, resolve } from "node:path";
import { typedAgentTools } from "../../test-support/agent-tools.mjs";
import { LIVE_MODEL_ID, configureLiveModel } from "../support/models.mjs";
import { MANAGED_HEADERS } from "../support/betas.mjs";
import {
  BACKEND,
  publishAgent,
  putAgent,
  putAgentResources,
  requireOk,
  uploadAgentFile,
} from "../support/control-plane.mjs";

const AGENT_ID = "repository-maintenance-agent";
const MODEL_ID = LIVE_MODEL_ID;
const ENVIRONMENT_ID = "env_local";
const DEPLOYMENT_NAME = "Repository maintenance brief";
const SNAPSHOT_PATH = "/mnt/files/repository-maintenance-snapshot.txt";
const TOOL_PATH = `/mnt/session/uploads${SNAPSHOT_PATH}`;
const RECORDING_FIXTURE_NAMES = new Set([
  DEPLOYMENT_NAME,
  "Recurring release decision",
  "Recurring release review",
]);

export const story = {
  job: "Recurring repository maintenance",
  stakes: "A repository can drift between reviews; the next maintenance pass must start from a fresh, attributable snapshot instead of remembered status.",
  handoff: "The completed Session keeps the repository revision, control checks, exceptions, and the next human decision together.",
  promise: "Convert a verified repository snapshot into an exception-only maintenance brief that can run again on a schedule.",
  effect: "Run once starts a real Session, reads the mounted snapshot, and reports the exact revision and tracked-change count without changing the repository.",
  aha: "Every scheduled run keeps the exact repository snapshot behind its exception brief.",
  loyalty: "The maintenance check becomes repeatable work while every result remains independently inspectable.",
  satisfaction: "The schedule, source snapshot, Agent read trace, and finished brief stay linked through one Session.",
  advocacy: "A reviewer can verify the facts behind each maintenance brief before deciding what to change.",
};

let snapshot;

function collectRepositorySnapshot() {
  const repository = resolve(process.env.AWAKEN_RECORD_SOURCE_REPO ?? resolve(import.meta.dirname, "../../.."));
  const git = (...args) => execFileSync("git", ["-C", repository, ...args], { encoding: "utf8", timeout: 10_000 }).trim();
  const revision = git("rev-parse", "HEAD");
  const branch = git("branch", "--show-current") || "detached";
  const trackedChanges = git("status", "--porcelain", "--untracked-files=no").split("\n").filter(Boolean).length;
  const trackedFiles = git("ls-files").split("\n").filter(Boolean);
  const cargoManifests = trackedFiles.filter((path) => path === "Cargo.toml" || path.endsWith("/Cargo.toml")).length;
  const workflows = trackedFiles.filter((path) => path.startsWith(".github/workflows/") && path.endsWith(".yml"));
  const present = (path) => trackedFiles.includes(path) ? "PRESENT" : "MISSING";
  const content = `REPOSITORY MAINTENANCE SNAPSHOT

SOURCE
- Repository: ${basename(repository)}
- Revision: ${revision}
- Branch: ${branch}
- Tracked files: ${trackedFiles.length}
- Tracked modifications: ${trackedChanges}
- Cargo manifests: ${cargoManifests}
- CI workflows: ${workflows.length}

CONTROLS
- Cargo.lock: ${present("Cargo.lock")}
- Security workflow: ${present(".github/workflows/security.yml")}
- Test workflow: ${present(".github/workflows/test.yml")}

REVIEW POLICY
- Report the exact revision and tracked modification count.
- List only missing controls or non-zero tracked modifications as maintenance exceptions.
- Do not infer file contents, owners, deadlines, or customer impact.
- Recommend a human review when an exception exists. Do not modify, commit, or deploy anything.`;
  return { repository, revision, branch, trackedChanges, content };
}

export async function prepare({ page }) {
  await configureLiveModel(page);
  snapshot = collectRepositorySnapshot();
}

export async function run({ page, goto, intro, say, clearCaption, checkpoint, runtimeCheckpoint, aha, expect, click, type, wait }) {
  if (!snapshot) snapshot = collectRepositorySnapshot();
  const file = await uploadAgentFile(page, {
    name: "repository-maintenance-snapshot.txt",
    content: snapshot.content,
  });
  await putAgent(page, AGENT_ID, {
      id: AGENT_ID, name: "Repository maintenance agent", model: { id: MODEL_ID },
      system: `First call read exactly once on ${TOOL_PATH}. Use only that snapshot. Write exactly these headings: Snapshot, Maintenance exceptions, Controls present, Next action. Under Snapshot, include the exact Revision and Tracked modifications values. If no exception exists, write None. Do not invent an owner or consequence, and do not claim to modify the repository. Stay under 120 words.`,
      // Snapshot decision rule: C1 the maintenance source is mounted read-only;
      // E1 exactly one typed Agent ToolSet enables the sole required `read` call.
      tools: typedAgentTools(["read"]), mcp_servers: [], skills: [],
      plugins: [], plugin_config: {}, context_policy: { kind: "keep_all" }, max_steps: 4,
  });
  await putAgentResources(page, AGENT_ID, [
    { binding_id: "repository-snapshot", target: { kind: "file", id: file.id }, mount_path: SNAPSHOT_PATH, access: "read_only" },
  ]);
  await publishAgent(page, AGENT_ID);
  const existing = await page.request.get(`${BACKEND}/v1/deployments`, { headers: MANAGED_HEADERS });
  await requireOk(existing, "Deployment fixture inventory");
  for (const deployment of (await existing.json()).data ?? []) {
    if (RECORDING_FIXTURE_NAMES.has(deployment.name)) {
      const archived = await page.request.post(
        `${BACKEND}/v1/deployments/${deployment.id}/archive`,
        { headers: MANAGED_HEADERS },
      );
      await requireOk(archived, `archive earlier recording Deployment ${deployment.id}`);
    }
  }
  await goto("/w/default/deployments");
  await intro(
    "Repository maintenance should use the current revision and controls, not last week's remembered status.",
    "A Deployment creates a separate Session and inspectable exception brief for each fresh snapshot.",
  );
  await say("This snapshot comes from the shown checkout. The Agent runs live.", 3200);
  await click(page.getByRole("button", { name: /New deployment|新建部署/ }));
  const modal = page.locator(".modal");
  await type(modal.getByPlaceholder("nightly-report"), DEPLOYMENT_NAME);
  await modal.locator("select").nth(0).selectOption(AGENT_ID);
  await modal.locator("select").nth(1).selectOption(ENVIRONMENT_ID);
  await type(modal.getByPlaceholder("0 20 * * 5"), "0 9 * * 1");
  await type(modal.getByPlaceholder("UTC"), "Asia/Shanghai");
  await say("Each run reports its revision, controls, and reviewable exceptions.", 3000);
  await type(modal.locator("textarea"), "Read the mounted repository maintenance snapshot and prepare the exception-only brief. Preserve the exact revision and tracked modification count.", { delay: 2 });
  await click(modal.getByRole("button", { name: /Create|创建/, exact: true }));

  const row = page.locator("tr").filter({ hasText: DEPLOYMENT_NAME });
  await checkpoint("the standing operation persists its schedule and environment", async () => {
    await expect(row).toContainText("0 9 * * 1");
    const response = await page.request.get(`${BACKEND}/v1/deployments`, { headers: MANAGED_HEADERS });
    await requireOk(response, "Deployment readback");
    const body = await response.json();
    expect(body.data).toEqual(expect.arrayContaining([
      expect.objectContaining({ name: DEPLOYMENT_NAME, environment_id: ENVIRONMENT_ID }),
    ]));
  });

  await say("Run once uses the Agent, Environment, task, and saved schedule.", 3200);
  const responsePromise = page.waitForResponse((response) => response.request().method() === "POST" && /\/deployments\/[^/]+\/run$/.test(new URL(response.url()).pathname));
  await click(row.getByRole("button", { name: /Run once|立即运行/, exact: true }));
  const runResponse = await responsePromise;
  expect(runResponse.ok()).toBeTruthy();
  const deploymentRun = await runResponse.json();
  const runRow = page.locator("tr").filter({ hasText: deploymentRun.id });
  await say("The trigger creates an inspectable Session with its snapshot and result.", 3000);
  await runtimeCheckpoint("the trigger creates a real Session with the Agent's output", async () => {
    await expect(page.getByText(/Deployment run created\.|部署运行已创建。/)).toBeVisible();
    await expect(runRow).toHaveCount(1);
    await expect(runRow.getByRole("link", { name: /Open Session|打开 Session/ })).toHaveAttribute(
      "href",
      `/w/default/sessions/${deploymentRun.session_id}`,
    );
    expect(deploymentRun.session_id).toMatch(/^sesn_/);
    const response = await page.request.get(`${BACKEND}/v1/deployment_runs?deployment_id=${deploymentRun.deployment_id}`, { headers: MANAGED_HEADERS });
    await requireOk(response, "Deployment Run readback");
    const body = await response.json();
    expect(body.data).toEqual(expect.arrayContaining([
      expect.objectContaining({ id: deploymentRun.id, session_id: deploymentRun.session_id, error: null }),
    ]));
    const session = await page.request.get(`${BACKEND}/v1/sessions/${deploymentRun.session_id}`, { headers: MANAGED_HEADERS });
    await requireOk(session, "Deployment Session readback");
    const sessionBody = await session.json();
    expect(sessionBody.deployment_id).toBe(deploymentRun.deployment_id);
    await expect.poll(async () => {
      const events = await page.request.get(`${BACKEND}/v1/sessions/${deploymentRun.session_id}/events`, { headers: MANAGED_HEADERS });
      await requireOk(events, "Deployment Session events readback");
      const eventBody = await events.json();
      const terminalError = eventBody.data.find((event) => event.type === "session.error");
      if (terminalError) throw new Error(`Deployment Session failed: ${JSON.stringify(terminalError)}`);
      return eventBody.data.some((event) =>
        event.type === "agent.message" &&
        (event.content ?? []).some((content) =>
          new RegExp(`Snapshot[\\s\\S]*${snapshot.revision.slice(0, 12)}[\\s\\S]*Tracked modifications[\\s\\S]*${snapshot.trackedChanges}[\\s\\S]*Maintenance exceptions[\\s\\S]*Next action`, "i").test(content.text ?? ""))
      );
    }, { timeout: 300_000 }).toBe(true);
  });
  await click(runRow.getByRole("link", { name: /Open Session|打开 Session/ }));
  await expect(page.getByText(/Read the mounted repository maintenance snapshot/i)).toBeVisible({ timeout: 15_000 });
  const brief = page.locator('[data-role="assistant"]').last();
  await expect(brief).toContainText(/Snapshot/i, { timeout: 15_000 });
  await expect(brief).toContainText(snapshot.revision.slice(0, 12));
  await expect(brief).toContainText(new RegExp(`Tracked modifications[^0-9]*${snapshot.trackedChanges}`, "i"));
  await expect(brief).toContainText(/Maintenance exceptions/i);
  await expect(brief).toContainText(/Next action/i);
  await clearCaption();
  await aha(story.aha);
  await wait(900);
  await clearCaption();
}
