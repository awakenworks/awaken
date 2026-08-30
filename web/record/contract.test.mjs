import assert from "node:assert/strict";
import { readdirSync, readFileSync } from "node:fs";
import test from "node:test";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { MARKETING_STORIES, PRODUCT_PROOFS, TARGET_MARKETING_STORIES } from "./catalog.mjs";

const here = dirname(fileURLToPath(import.meta.url));
const flowsDir = resolve(here, "flows");
const proofsDir = resolve(here, "proofs");
const flows = readdirSync(flowsDir).filter((name) => name.endsWith(".mjs")).sort();

function executableSources(root) {
  return readdirSync(root, { withFileTypes: true }).flatMap((entry) => {
    const path = resolve(root, entry.name);
    if (entry.isDirectory()) return executableSources(path);
    return /\.(?:mjs|ts|tsx)$/.test(entry.name) ? [path] : [];
  });
}

test("every recording is a story-shaped executable test", () => {
  assert.ok(flows.length > 0, "at least one recording flow exists");
  for (const name of flows) {
    const source = readFileSync(resolve(flowsDir, name), "utf8");
    assert.match(source, /await intro\(/, `${name}: explain intent and capability before operating`);
    assert.match(source, /await (?:checkpoint|runtimeCheckpoint)\(/, `${name}: assert at least one product claim`);
    assert.match(source, /await aha\(/, `${name}: land a visible, shareable payoff`);
    assert.match(
      source,
      /(?:controlled test data|fixed test data|fixed contract data|snapshot comes from|deterministic response|response is deterministic)[\s\S]{0,180}(?:live run|live\.|are live|are real)/i,
      `${name}: disclose the source of scenario facts and distinguish it from the live product run`,
    );
    assert.match(source, /export const story\s*=\s*{/, `${name}: declare one customer story`);
    for (const field of ["job", "stakes", "handoff", "promise", "effect", "aha", "loyalty", "satisfaction", "advocacy"]) {
      assert.match(source, new RegExp(`${field}:\\s*["']`), `${name}: story.${field} is required`);
    }
  }
});

test("public video material uses Awaken Agents as the only Awaken product identity", () => {
  const publicSources = [
    resolve(here, "catalog.mjs"),
    resolve(here, "README.md"),
    resolve(here, "VIDEO_STRATEGY.md"),
    ...flows.map((name) => resolve(flowsDir, name)),
  ];
  const retiredIdentity = /Harness Runtime Platform|Awaken Runtime|Agents Runtime|Awaken Harness|platform-overview|Platform Operations/;
  for (const path of publicSources) {
    assert.doesNotMatch(readFileSync(path, "utf8"), retiredIdentity, `${path}: use Awaken Agents as the public product identity`);
  }
});

test("every executable Files upload uses the official file-only multipart shape", () => {
  const roots = [resolve(here, "support"), proofsDir, flowsDir, resolve(here, "../e2e"), resolve(here, "../scripts")];
  for (const path of roots.flatMap(executableSources)) {
    const source = readFileSync(path, "utf8");
    assert.doesNotMatch(source, /purpose:\s*["']agent["']/, `${path}: Files upload must not send the unsupported OpenAI-style purpose field`);
  }
});

test("published duration follows the story while live execution stays bounded", () => {
  const harness = readFileSync(resolve(here, "harness.mjs"), "utf8");
  const batch = readFileSync(resolve(here, "record-all.mjs"), "utf8");
  const manifest = readFileSync(resolve(here, "release-manifest.mjs"), "utf8");
  assert.match(harness, /MAX_FLOW_MS = 480_000/);
  assert.doesNotMatch(harness, /every story must close within/);
  assert.match(harness, /\(\) => page\?\.close\(\)/);
  assert.match(harness, /MAX_PREPARE_MS = 480_000/);
  assert.match(harness, /MAX_CONTEXT_CLOSE_MS = 30_000/);
  assert.match(harness, /MAX_CLOSE_MS = 300_000/);
  assert.match(harness, /video probe remains publication authority/);
  assert.match(harness, /MAX_ENCODE_MS = 600_000/);
  assert.match(harness, /"-preset", "superfast"/);
  assert.match(harness, /MAX_PROBE_MS = 15_000/);
  assert.match(harness, /MAX_PROCESS_MS = 1_200_000/);
  assert.match(harness, /function activeDeadline\(/);
  assert.match(harness, /gap <= 5_000 \? gap : 1_000/);
  assert.match(harness, /spawn\("\/usr\/bin\/caffeinate", \["-dims", "-w", String\(process\.pid\)\]/);
  assert.match(harness, /wallGap > 5_000/);
  assert.match(harness, /recording invalidated by system suspension or an unresponsive event loop/);
  assert.doesNotMatch(harness, /wallGap - responsiveGap/);
  assert.match(harness, /captions exceed video duration/);
  assert.match(harness, /not\(between\(t/);
  assert.match(harness, /select=\$\{suspensionFilter\}/);
  assert.match(harness, /timelineNow\(\) - videoStartedActiveAt/);
  assert.match(harness, /editorial cut: removed/);
  assert.match(harness, /activeDuration > 8_000/);
  assert.match(harness, /process\.exit\(124\)/);
  assert.match(harness, /execFileActive\([\s\S]*"ffmpeg"[\s\S]*MAX_ENCODE_MS/);
  assert.match(harness, /execFileActive\([\s\S]*"ffprobe"[\s\S]*MAX_PROBE_MS/);
  assert.match(harness, /→ video encode/);
  assert.match(harness, /→ video quality probe/);
  assert.match(harness, /\[record\] → \$\{name\}/);
  assert.match(harness, /expected exactly one AHA/);
  assert.match(harness, /runtime failed before the claimed effect/);
  assert.match(batch, /MAX_CHAPTER_PROCESS_MS = 1_800_000/);
  assert.match(batch, /timeout: MAX_CHAPTER_PROCESS_MS/);
  assert.match(batch, /killSignal: "SIGKILL"/);
  assert.doesNotMatch(manifest, /max_duration_seconds|duration_seconds < 180/);
});

test("clean installation recording uses the release artifact", () => {
  const installer = readFileSync(resolve(here, "install-all-in-one.mjs"), "utf8");
  assert.match(installer, /target\/release\/awaken/);
  assert.doesNotMatch(installer, /target\/debug\/awaken/);
  assert.match(installer, /const serviceEnvironment = \{ \.\.\.process\.env \}/);
  assert.match(installer, /delete serviceEnvironment\[name\]/);
  assert.match(installer, /env: serviceEnvironment/);
});

// Recording FMECA cause/effect graph:
// C1=current flow passes, C2=FFmpeg succeeds, C3=a prior artifact exists,
// C4=focused evidence is low in the viewport, C5=the recorded page exists.
// E1=one atomic current MP4, E2=non-zero + diagnostic WebM, E3=no stale success,
// E4=caption/proof move away from evidence, E5=captions share the video epoch.
// Decision rules: 11***→E1; 10***→E2; **1**→E3; ***1*→E4; ****1→E5.
test("recording publication fails closed and cannot reuse a stale success", () => {
  const harness = readFileSync(resolve(here, "harness.mjs"), "utf8");
  const manifest = readFileSync(resolve(here, "release-manifest.mjs"), "utf8");
  const batch = readFileSync(resolve(here, "record-all.mjs"), "utf8");
  assert.match(harness, /for \(const artifact of \[slug, `\$\{slug\}\.failed`\]\)/);
  assert.match(harness, /\.mp4\.tmp\.mp4/);
  assert.match(harness, /renameSync\(temporaryMp4, mp4\)/);
  assert.match(harness, /ffmpeg failed:/);
  assert.match(harness, /failed = true;/);
  assert.match(harness, /failure: failureReason/);
  assert.match(harness, /flow_source_sha256: flowSourceSha256/);
  assert.match(harness, /harness_sha256: harnessSha256/);
  assert.match(manifest, /story\.flow_source_sha256 !== flowSha256/);
  assert.match(manifest, /story\.harness_sha256 !== harnessSha256/);
  assert.match(manifest, /recorded from an older story or harness/);
  assert.match(manifest, /story\.product_revision !== currentProductRevision/);
  assert.match(manifest, /current_product_revision: currentProductRevision/);
  assert.match(batch, /currentArtifacts\.has\(slug\)/);
  assert.doesNotMatch(batch, /existsSync\(resolve\(out, `\$\{slug\}\.mp4`\)\)/);
});

test("recording setup effects are acknowledged and repeat-safe", () => {
  const corpus = flows.map((name) => readFileSync(resolve(flowsDir, name), "utf8")).join("\n");
  const helpers = readFileSync(resolve(here, "support/control-plane.mjs"), "utf8");
  const matrix = readFileSync(resolve(here, "ROOT_CAUSE_MATRIX.md"), "utf8");

  // C1 unchecked setup mutation -> missing prerequisite -> late, misleading UI
  // failure. C2 stable fixture + create-only/fixed revision -> conflict on rerun.
  // E1 every direct setup mutation is routed through a checked helper; E2 Skill
  // conflicts reuse the returned id; E3 Resource bindings advance current CAS.
  assert.doesNotMatch(corpus, /^\s*await page\.request\.(?:post|put|patch|delete)/m);
  assert.doesNotMatch(corpus, /await \(await page\.request\.(?:post|put|patch|delete)/);
  assert.match(helpers, /if \(!response\.ok\(\)\)/);
  assert.match(helpers, /response\.status\(\) !== 409/);
  assert.match(helpers, /conflict\.error\?\.message/);
  assert.match(helpers, /skill `\(skill_\[\^`\]\+\)`/);
  assert.match(helpers, /revision = Number\(\(await current\.json\(\)\)\.revision \?\? 0\) \+ 1/);
  assert.match(matrix, /Root cause/);
  assert.match(matrix, /Causal\/negative tests/);
});

test("the scheduled story removes its own stale fixtures before recording", () => {
  const deployment = readFileSync(resolve(flowsDir, "06-scheduled-operation.mjs"), "utf8");
  assert.match(deployment, /RECORDING_FIXTURE_NAMES\.has\(deployment\.name\)/);
  assert.match(deployment, /"Recurring release decision"/);
  assert.match(deployment, /\/deployments\/\$\{deployment\.id\}\/archive/);
  assert.match(deployment, /const ENVIRONMENT_ID = "env_local"/);
  assert.match(deployment, /AWAKEN_RECORD_SOURCE_REPO/);
  assert.match(deployment, /git\("status", "--porcelain", "--untracked-files=no"\)/);
  assert.match(deployment, /mount_path: SNAPSHOT_PATH, access: "read_only"/);
  assert.match(deployment, /const runRow = page\.locator\("tr"\)\.filter\(\{ hasText: deploymentRun\.id \}\)/);
  assert.match(deployment, /click\(runRow\.getByRole\("link"/);
  assert.doesNotMatch(deployment, /click\(page\.getByRole\("link", \{ name: \/Open Session/);
  assert.doesNotMatch(deployment, /HOLD|NO-GO|Billing|rollback risk/);
  assert.doesNotMatch(deployment, /const DEPLOYMENT_NAME = `[^`]*Date\.now/);
});

test("Session resources, control, and Environment placement match executable all-in-one evidence", () => {
  const overview = readFileSync(resolve(flowsDir, "00-awaken-agents-overview.mjs"), "utf8");
  const control = readFileSync(resolve(proofsDir, "14-session-control.mjs"), "utf8");
  // A release all-in-one always realizes env_local. Merely authoring a cloud
  // Environment does not make a Worker available, so marketing runs must bind
  // to the executable Environment instead of waiting forever in rescheduling.
  assert.doesNotMatch(overview, /createEnvironment|type: "cloud"/);
  assert.match(readFileSync(resolve(here, "support/control-plane.mjs"), "utf8"), /environment_id: "env_local"/);
  assert.match(control, /event\.type === "user\.message"/);
  assert.match(control, /toBeEnabled\(\)/);
  assert.match(control, /Stop run\|停止运行/);
  assert.match(control, /getByRole\("alertdialog"\)/);
  assert.ok([...control.matchAll(/getByRole\("alertdialog"\)/g)].length >= 2);
  assert.match(control, /locator\("\.pill"\).*\^archived\$/);
  assert.doesNotMatch(control, /getByRole\("dialog"\)/);
  assert.doesNotMatch(control, /name: \/interrupt/i);
  assert.doesNotMatch(control, /waitForResponse/);
  assert.match(control, /event\.type === "user\.interrupt"/);
  assert.match(overview, /target: \{ kind: "file", id: file\.id \}/);
  assert.match(overview, /mount_path: EVIDENCE_PATH, access: "read_only"/);
  assert.match(overview, /event\.type === "agent\.tool_use" && event\.name === "read"/);
  assert.match(overview, /getByRole\("region", \{ name: \/Tool read\|工具 read\//);
  assert.doesNotMatch(overview, /details\[data-tool=/);
  assert.match(control, /getByPlaceholder\(\/Message\|输入消息\/\)/);
  assert.doesNotMatch(overview, /const runRequest = page\.request\.post/);
  assert.doesNotMatch(control, /const runRequest = page\.request\.post/);
  const models = readFileSync(resolve(here, "support/models.mjs"), "utf8");
  assert.match(models, /AWAKEN_RECORD_SYNTHETIC_PORT \?\? 38088/);
  assert.match(models, /record-synthetic-v2-/);
  assert.match(models, /"content-type": "text\/event-stream"/);
  assert.match(models, /writeAnthropicEvent\(response, "message_start"/);
  assert.match(models, /event: ping\\ndata:/);
  assert.match(models, /syntheticModel !== "session-control-recording-model"/);
  assert.match(models, /event: content_block_stop/);
  assert.match(models, /event: message_delta/);
  assert.match(models, /event: message_stop/);
  assert.doesNotMatch(models, /: keep-alive/);
});

test("recording chrome shares the video timeline and avoids focused evidence", () => {
  const harness = readFileSync(resolve(here, "harness.mjs"), "utf8");
  assert.match(harness, /const SIZE = \{ width: 1920, height: 1200 \}/);
  assert.match(harness, /recording requires one all-in-one origin/);
  assert.match(harness, /the API listener did not serve the embedded production console/);
  assert.match(harness, /"-vf", videoFilter/);
  assert.match(harness, /"-crf", "14"/);
  assert.match(harness, /verifyFinalVideo\(temporaryMp4\)/);
  assert.match(harness, /video quality gate failed/);
  assert.match(harness, /videoStartedAt = Date\.now\(\)/);
  assert.match(harness, /startMs = timelineNow\(\) - videoStartedActiveAt/);
  assert.doesNotMatch(harness, /Intent ·|Awaken ·|AHA ·/);
  assert.match(harness, /classList\.toggle\("top", targetIsLow\)/);
  assert.match(harness, /classList\.toggle\("low", targetIsLow\)/);
  assert.match(harness, /Recording stopped/);
  assert.match(harness, /ctx\.setDefaultTimeout\(15_000\)/);
  assert.match(harness, /ctx\.setDefaultNavigationTimeout\(20_000\)/);
  assert.match(harness, /text\.length <= \(opts\.sequentialLimit \?\? 80\)/);
  assert.match(harness, /await locator\.fill\(text\)/);
  assert.ok(harness.indexOf("await mod.prepare") < harness.indexOf("page = await ctx.newPage()"));
  assert.match(harness, /page = await ctx\.newPage\(\);\s+api\.page = page;/);
  assert.match(harness, /setup latency and a blank canvas never enter the release/);
  assert.doesNotMatch(harness, /\.rec-focus\{position:relative;z-index:/);
  assert.match(harness, /Evidence focus must never become an input authority/);
  assert.match(harness, /flowRun\?\.catch\(\(\) => \{\}\)/);
  assert.match(harness, /late page-closed failures cannot escape as an/);
});

test("live Provider discovery completes before the visible case begins", () => {
  const liveCases = [
    [proofsDir, "03-tools-permissions.mjs"],
    [proofsDir, "04-resources-transparency.mjs"],
    [proofsDir, "06-ai-state-machine.mjs"],
    [flowsDir, "03-connect-anthropic-sdk.mjs"],
    [flowsDir, "06-scheduled-operation.mjs"],
  ];
  for (const [directory, name] of liveCases) {
    const source = readFileSync(resolve(directory, name), "utf8");
    const prepare = source.slice(source.indexOf("export async function prepare"), source.indexOf("export async function run"));
    assert.match(prepare, /await configureLiveModel\(page\);/, `${name}: external Provider directory latency belongs before recording`);
    const run = source.slice(source.indexOf("export async function run"));
    assert.doesNotMatch(run, /configureLiveModel\(page\)/, `${name}: do not sync the Provider on video`);
    if (name === "06-ai-state-machine.mjs") {
      assert.ok(prepare.indexOf("configureLiveModel(page)") < prepare.indexOf("putAgent(page, AGENT_ID, DRAFT)"));
      assert.doesNotMatch(run, /putAgent\(page, AGENT_ID, DRAFT\)/);
    }
  }
});

test("live Provider discovery retries only bounded transient failures", () => {
  const models = readFileSync(resolve(here, "support/models.mjs"), "utf8");
  assert.match(models, /attempt <= 3/);
  assert.match(models, /\[429, 502, 503, 504\]\.includes\(connected\.status\(\)\)/);
  assert.match(models, /delay\(attempt \* 1_000\)/);
  assert.doesNotMatch(models, /page\.waitForTimeout\(attempt \* 1_000\)/);
});

test("the recording browser uses the one-time local setup handoff, not a service credential", () => {
  const harness = readFileSync(resolve(here, "harness.mjs"), "utf8");
  const proofHarness = readFileSync(resolve(here, "proof-harness.mjs"), "utf8");
  const models = readFileSync(resolve(here, "support/models.mjs"), "utf8");
  assert.match(harness, /AWAKEN_RECORD_SETUP_TOKEN/);
  assert.match(harness, /\/v1\/auth\/local\/exchange/);
  assert.match(harness, /HttpOnly browser/);
  assert.doesNotMatch(harness, /admin-token/);
  assert.match(proofHarness, /exchange\.status\(\) === 401/);
  assert.match(proofHarness, /first\s+process consumes it and persists the HttpOnly session/);
  assert.match(proofHarness, /proof browser is not authenticated/);
  assert.match(models, /\/v1\/config\/workspace-context/);
  assert.doesNotMatch(models, /wrkspc_default/);
});

test("browser operations and API checkpoints share one backend authority", () => {
  const harness = readFileSync(resolve(here, "harness.mjs"), "utf8");
  const controlPlane = readFileSync(resolve(here, "support/control-plane.mjs"), "utf8");
  assert.match(harness, /process\.env\.BACKEND_URL/);
  assert.match(controlPlane, /process\.env\.BACKEND_URL \?\? process\.env\.AWAKEN_HTTP_URL/);
  assert.match(controlPlane, /browser operate one all-in-one instance while a/);
});

test("response listeners ignore workspace path prefixes and query parameters", () => {
  const corpus = [
    ...flows.map((name) => readFileSync(resolve(flowsDir, name), "utf8")),
    ...PRODUCT_PROOFS.map((slug) => readFileSync(resolve(proofsDir, `${slug}.mjs`), "utf8")),
  ].join("\n");
  assert.doesNotMatch(corpus, /response\.url\(\)\.endsWith/);
  assert.match(corpus, /new URL\(response\.url\(\)\)\.pathname/);
  assert.doesNotMatch(corpus, /pathname\.endsWith\([`"']\/v1\//);
  assert.doesNotMatch(corpus, /\/\\\/v1\\\/(?:config|sessions|deployments)/);
  assert.match(corpus, /pathname\.endsWith\(`\/config\/agents\/\$\{AGENT_ID\}\/publish`\)/);
  assert.match(corpus, /pathname\.endsWith\("\/sessions"\)/);
  assert.match(corpus, /\/\\\/deployments\\\/\[\^\/\]\+\\\/run\$/);
});

test("publication cases prove request dispatch and bound the response wait", () => {
  for (const [directory, name] of [[flowsDir, "02-build-agent.mjs"], [proofsDir, "04-resources-transparency.mjs"]]) {
    const flow = readFileSync(resolve(directory, name), "utf8");
    assert.match(flow, /waitForRequest\(isPublishRequest, \{ timeout: 60_000 \}\)/);
    assert.match(flow, /waitForResponse\([\s\S]*\{ timeout: 60_000 \}/);
    assert.match(flow, /await publishRequest;[\s\S]*await publishResponse/);
  }
});

test("the installation chapter owns a clean all-in-one lifecycle", () => {
  const flow = readFileSync(resolve(proofsDir, "07-all-in-one-startup.mjs"), "utf8");
  const launcher = readFileSync(resolve(here, "install-all-in-one.mjs"), "utf8");
  assert.match(launcher, /mkdtempSync/);
  assert.match(launcher, /copyFileSync\(sourceBinary, binary\)/);
  assert.match(launcher, /spawn\(binary, \["all-in-one", "--config", config\]/);
  assert.match(launcher, /BACKEND_URL: origin/);
  assert.match(launcher, /AWAKEN_RECORD_STARTUP_RECEIPT/);
  assert.match(launcher, /AWAKEN_RECORD_SETUP_TOKEN: ""/);
  assert.match(flow, /Captured all-in-one startup receipt/);
  assert.match(flow, /fetch\(`\$\{BACKEND\}\/readyz`\)/);
  assert.match(flow, /await goto\("\/w\/default\/overview"\)/);
});

test("the Access proof cannot inherit the local-admin browser cookie", () => {
  const source = readFileSync(resolve(proofsDir, "16-access-boundary.mjs"), "utf8");
  const surface = readFileSync(resolve(here, "../src/surfaces/access.tsx"), "utf8");
  assert.match(source, /requestWithOnlyServiceKey/);
  assert.match(source, /Node's fetch rather than the Playwright request context/);
  assert.doesNotMatch(source, /page\.request/);
  assert.match(source, /Service principal\|\u670d\u52a1\u4e3b\u4f53/);
  assert.match(source, /Create key\|\u521b\u5efa Key/);
  assert.match(source, /getByRole\("alertdialog"\)/);
  assert.match(source, /expect\(secretBanner\)\.toBeHidden/);
  assert.match(source, /toContain\(response\.status\)/);
  assert.match(source, /requireFromE2e\("@anthropic-ai\/sdk"\)/);
  assert.match(source, /selectOption\("workspace_admin"\)/);
  assert.match(source, /sdk\.beta\.sessions\.create/);
  assert.match(source, /sdk\.beta\.sessions\.retrieve/);
  assert.match(source, /Console opens the exact Session created by the official SDK/);
  assert.match(surface, /current\?\.id === id \? null : current/);
  assert.match(surface, /return mintedServiceKey\(response\)/);
  assert.match(surface, /response\.api_token\?\.id/);
  assert.doesNotMatch(surface, /JSON\.stringify\(r\)/);
  assert.match(surface, /cannot create Sessions through the SDK/);
  assert.match(surface, /value=\{minted\.secret\}/);
  assert.match(surface, /identity_mode=self-managed/);
  assert.doesNotMatch(surface, /AWAKEN_MGMT_IAM=embedded/);
});

test("the state-machine recording proves runtime enforcement, not just configuration", () => {
  const source = readFileSync(resolve(proofsDir, "06-ai-state-machine.mjs"), "utf8");
  assert.match(source, /Try draft\|试运行草稿/);
  assert.match(source, /Start preview\|开始预览/);
  assert.match(source, /State Machine blocks the unread write at runtime/);
  assert.match(source, /type: "agent\.tool_result"/);
  assert.match(source, /tool_use_id: deniedWrite\.id/);
  assert.match(source, /permission_policy: \{ type: "always_allow" \}/);
  assert.doesNotMatch(source, /Agent working\|Agent 工作中/);
  assert.match(source, /previewTranscript\.locator\('details\[data-tool="write"\]'\)/);
  assert.match(source, /writeCard\.locator\("summary"\)\.click\(\)/);
  assert.doesNotMatch(source, /:scope > summary/);
  assert.match(
    source,
    /toContainText\(\/blocked\|Read \.[*] before writing\|denied\/i, \{ timeout: 60_000 \}\)/,
  );
});

test("interactive transcripts follow committed SSE frames without a manual refresh", () => {
  const transcript = readFileSync(resolve(here, "../src/components/session/Transcript.tsx"), "utf8");
  const hook = readFileSync(resolve(here, "../src/lib/useSessionLog.ts"), "utf8");
  assert.match(transcript, /useSessionLog\(base, queryKey, \{[\s\S]*followLive: viewProps\.composer \?\? true/);
  assert.match(hook, /if \(followLive\)/);
  assert.match(hook, /setQueryData<SessionEvent\[\]>/);
});

test("streaming marketing results wait for the complete decision, not the first delta", () => {
  const agent = readFileSync(resolve(flowsDir, "02-build-agent.mjs"), "utf8");
  assert.match(agent, /Compatibility\[\\s\\S\]\*\(\?:BREAKING\|incompatible\)\[\\s\\S\]\*Breaking changes/);
  assert.match(agent, /agent-preview-conversation:not\(\[hidden\]\) \[data-role="assistant"\]/);
  assert.doesNotMatch(agent, /\.transcript \[data-role="assistant"\]/);
  assert.match(agent, /activeProtocolPreview\.getByText\(FIRST_TASK/);
  assert.doesNotMatch(agent, /page\.getByText\(FIRST_TASK/);
  assert.match(agent, /\{ timeout: 60_000 \}/);
  assert.match(agent, /agent-working.*not\.toBeVisible\(\{ timeout: 60_000 \}\)/);
  assert.doesNotMatch(agent, /toContainText\(\/Breaking changes\/i\);/);
});

test("overview captions point to visible UI evidence without long static waits", () => {
  const overview = readFileSync(resolve(flowsDir, "00-awaken-agents-overview.mjs"), "utf8");
  const harness = readFileSync(resolve(here, "harness.mjs"), "utf8");
  const beats = [...overview.matchAll(/await beat\(/g)].length;
  assert.ok(beats >= 3 && beats <= 4, "the opener should prove one outcome without becoming a route tour");
  assert.equal([...overview.matchAll(/await goto\(/g)].length, 2, "the opener must reopen the same durable work record");
  assert.match(overview, /the decision and its source survive a fresh navigation/);
  assert.match(overview, /HOLD\[\\s\\S\]\*INV-204/);
  assert.match(overview, /&& \/UNASSIGNED\/i\.test\(text\)/);
  assert.match(overview, /&& \/STALE\/i\.test\(text\)/);
  assert.doesNotMatch(overview, /INV-204\[\\s\\S\]\*\(\?:UNASSIGNED/);
  assert.match(harness, /\.rec-focus/);
  assert.match(harness, /waitFor\(\{ state: "visible", timeout: 10_000 \}\)/);
  assert.match(harness, /scrollIntoViewIfNeeded\(\{ timeout: 10_000 \}\)/);
  assert.match(harness, /Math\.min\(5200,/);
  assert.match(harness, /trackSubtitle\(text, holdMs\);[\s\S]*rec-cap[\s\S]*classList\.remove\("on"\)/);
});

test("tool governance, tool presentation, and Memory each have the right evidence owner", () => {
  const tools = readFileSync(resolve(proofsDir, "03-tools-permissions.mjs"), "utf8");
  const presentation = readFileSync(resolve(proofsDir, "20-tool-presentation.mjs"), "utf8");
  const memory = readFileSync(resolve(proofsDir, "04-resources-transparency.mjs"), "utf8");
  assert.match(tools, /runtime denies the matching bash call before execution/);
  assert.match(tools, /Never retry a denied tool call/);
  assert.match(tools, /DELETE BLOCKED BY POLICY/);
  assert.match(tools, /agent\.tool_use" && event\.name === "bash"\)\)\.toHaveLength\(1\)/);
  assert.match(presentation, /mcp__issues__create_issue/);
  assert.match(presentation, /config\.tools\)\.toEqual\(\["read"\]\)/);
  assert.match(presentation, /Show this tool to the model on demand/);
  assert.match(presentation, /exposure: "on_demand"/);
  assert.doesNotMatch(presentation, /Defer this tool|defer: true/);
  assert.ok([...memory.matchAll(/createManagedSession\(/g)].length >= 2);
  assert.match(memory, /\/archive/);
  assert.match(memory, /memories\?view=full/);
  assert.match(memory, /memory\.content/);
  assert.match(memory, /durablePolicy = "Human approval is required before every external side effect\."/);
  assert.match(memory, /tools: \["read", "write"\]/);
  assert.match(memory, /max_steps: 3/);
  assert.match(memory, /\.mnt\/mnt\/memory\/project\/release-policy\.txt/);
  assert.doesNotMatch(memory, /Save resources|保存资源/);
  assert.match(memory, /publication\.agent_inputs\.inputs/);
  assert.match(memory, /MEMORY_HEADERS/);
  assert.match(memory, /MANAGED_HEADERS/);
  assert.ok([...memory.matchAll(/toBeEnabled\(\{ timeout: 60_000 \}\)/g)].length >= 2);
  assert.match(memory, /Memory writer Session readback/);
  assert.match(memory, /Memory reader Session readback/);
  assert.doesNotMatch(memory, /agent-working/);
  assert.doesNotMatch(memory, /\/v1\/files\?scope_id/);
  assert.match(memory, /expect\(recalledText\)\.toMatch\(\/human\/i\)/);
  assert.match(memory, /expect\(recalledText\)\.toMatch\(\/approval\/i\)/);
  assert.match(memory, /expect\(recalledText\)\.toMatch\(\/external side effect\/i\)/);
});

test("Resource provenance uses the Session Inputs view, not the Workspace Files route", () => {
  const source = readFileSync(resolve(proofsDir, "11-resource-provenance.mjs"), "utf8");
  assert.match(source, /name: \/Inputs\|输入\//);
  assert.doesNotMatch(source, /name: \/Files\|文件\//);
  assert.match(source, /Session Resource realization receipt/);
  assert.match(source, /production-default Namespace sandbox/);
  assert.match(source, /Release only after checks pass\./);
  assert.match(source, /mount_path: "\/mnt\/files\/release-policy\.txt", access: "read_only"/);
  assert.ok(source.indexOf("Session Resource realization receipt") < source.indexOf('name: /Inputs|输入/'));
  assert.match(source, /expect\(composer\)\.toBeEnabled\(\{ timeout: 60_000 \}\)/);
});

test("the live-model connection proof asserts agent output, never page copy", () => {
  const model = readFileSync(resolve(proofsDir, "01-connect-model.mjs"), "utf8");
  assert.match(model, /\.model-test-modal \[data-role="assistant"\]/);
  assert.match(model, /getByText\("MODEL READY", \{ exact: true \}\)/);
  assert.match(model, /getByRole\("cell", \{ name: MODEL, exact: true \}\)/);
  assert.doesNotMatch(model, /locator\("tr", \{ hasText: MODEL \}\)/);
  assert.doesNotMatch(model, /document\.body\.innerText/);
  assert.match(model, /toBeVisible\(\{ timeout: 60_000 \}\)/);
});

test("the direct Managed API proof carries its explicit protocol version on create and read", () => {
  const source = readFileSync(resolve(proofsDir, "13-managed-api-ingress.mjs"), "utf8");
  assert.match(source, /import \{ MANAGED_HEADERS \}/);
  assert.match(source, /metadata: \{ source: "managed-api-proof" \},\s*\}, MANAGED_HEADERS\)/);
  assert.match(source, /sessions\/\$\{session\.id\}`, \{ headers: MANAGED_HEADERS \}/);
  assert.match(source, /getByText\("Managed API agent", \{ exact: true \}\)/);
  assert.match(source, /expect\(body\.agent\.id\)\.toBe\(AGENT_ID\)/);
  assert.match(source, /details\.technical-id/);
  assert.match(source, /Technical ID\|技术 ID/);
});

test("the SDK story uses the official client and closes the request-to-Console loop", () => {
  const source = readFileSync(resolve(flowsDir, "03-connect-anthropic-sdk.mjs"), "utf8");
  assert.match(source, /createRequire\(resolve\(import\.meta\.dirname, "\.\.\/\.\.\/\.\.\/e2e\/package\.json"\)\)/);
  assert.match(source, /requireFromE2e\("@anthropic-ai\/sdk"\)\.default/);
  assert.match(source, /client\.beta\.sessions\.create\(/);
  assert.match(source, /client\.beta\.sessions\.events\.send\(/);
  assert.match(source, /client\.beta\.sessions\.events\.list\(/);
  assert.match(source, /metadata: \{ source: "official-anthropic-sdk", request_marker: "SDK-HANDOFF-27" \}/);
  assert.match(source, /event\.id === acceptedEventId && event\.type === "user\.message"/);
  assert.match(source, /Result\[\\s\\S\]\*Integration boundary\[\\s\\S\]\*Next step/);
  assert.match(source, /SDK-HANDOFF-27\/i\.test\(text\)/);
  assert.match(source, /toContainText\("SDK-HANDOFF-27"/);
  assert.doesNotMatch(source, /Next step\[\\s\\S\]\*SDK-HANDOFF-27/);
  assert.match(source, /Create or manage service API keys/);
  assert.match(source, /awakenworks\.com\/docs\/agents\/protocols\/managed-agents/);
  assert.match(source, /if \(!\(await managedHelp\.evaluate\(\(element\) => element\.open\)\)\)/);
  assert.match(source, /details\.technical-id/);
  assert.match(source, /Technical ID\|技术 ID/);
  assert.doesNotMatch(source, /page\.request\.post\(`\$\{BACKEND\}\/v1\/sessions/);
});

test("the controlled-action story binds real source, approval, and artifact causally", () => {
  const source = readFileSync(resolve(flowsDir, "04-human-controlled-action.mjs"), "utf8");
  const models = readFileSync(resolve(here, "support/models.mjs"), "utf8");
  assert.match(source, /configureSyntheticModel\(page, MODEL_ID\)/);
  assert.match(models, /syntheticModel === "recording-repository-action-model"/);
  assert.match(models, /sendAnthropicToolUse\(response, readId, "read"/);
  assert.match(models, /sendAnthropicToolUse\(response, writeId, "write"/);
  assert.match(source, /name: "read", type: "read", enabled: true, permission_policy: \{ type: "always_allow" \}/);
  assert.match(source, /name: "write", type: "write", enabled: true, permission_policy: \{ type: "always_ask" \}/);
  assert.doesNotMatch(source, /plugin_config:[\s\S]*?permission:/);
  assert.match(source, /const SOURCE_RELATIVE = "web\/src\/surfaces\/protocols\.tsx"/);
  assert.match(source, /const ARTIFACT_PATH = `\/mnt\/session\/outputs\/\$\{ARTIFACT_NAME\}`/);
  assert.match(source, /execFileSync\("git", \["rev-parse", "--short=12", "HEAD"\]/);
  assert.match(source, /sourceSha256 = sha256\(source\)/);
  assert.match(source, /source\.includes\("CopyButton"\)/);
  assert.match(source, /mount_path: SOURCE_MOUNT, access: "read_only"/);
  assert.match(source, /call write exactly once now/);
  assert.match(source, /Awaken will pause that tool call before execution so a person can review it/);
  assert.match(source, /Do not approve or bypass the pending write yourself/);
  assert.doesNotMatch(source, /do not cross any write approval boundary yourself/);
  assert.match(source, /event\.evaluated_permission === "ask" && event\.input\?\.file_path === ARTIFACT_PATH/);
  assert.doesNotMatch(source, /event\.input\?\.path/);
  assert.match(source, /protected artifact write executed before human approval/);
  assert.match(source, /getByRole\("button", \{ name: \/Allow\|允许\/, exact: true \}\)/);
  assert.match(source, /user\.tool_confirmation/);
  assert.match(source, /agent\.tool_result/);
  assert.match(source, /\/v1\/files\/\$\{artifact\.id\}\/content/);
  assert.match(source, /expect\(text\)\.toContain\(sourceSha256\)/);
  assert.match(source, /locator\("\.session-detail-layout \.segmented"\)\.getByRole\("button", \{ name: \/Artifacts\|产物\//);
});

test("the restart story owns a release all-in-one and resumes one committed Session", () => {
  const source = readFileSync(resolve(flowsDir, "05-survive-restart.mjs"), "utf8");
  const harness = readFileSync(resolve(here, "harness.mjs"), "utf8");
  const models = readFileSync(resolve(here, "support/models.mjs"), "utf8");
  assert.match(source, /configureSyntheticModel\(page, MODEL_ID\)/);
  assert.match(models, /syntheticModel === "recording-restart-continuity-model"/);
  assert.match(models, /sendAnthropicToolUse\(response, writeId, "write"/);
  assert.match(models, /recovered handoff preserved marker RESTART-CONTINUITY-41/);
  assert.match(source, /name: "write", type: "write", enabled: true, permission_policy: \{ type: "always_ask" \}/);
  assert.doesNotMatch(source, /plugin_config:[\s\S]*?permission:/);
  assert.match(harness, /OWNS_RESTARTABLE_ALL_IN_ONE = slug === "05-survive-restart"/);
  assert.match(harness, /if \(OWNS_RESTARTABLE_ALL_IN_ONE\) \{[\s\S]*process\.env\.BACKEND_URL = BACKEND;[\s\S]*process\.env\.CONSOLE_URL = CONSOLE;/);
  assert.ok(harness.indexOf("process.env.BACKEND_URL = BACKEND") < harness.indexOf("await import(pathToFileURL"));
  assert.match(harness, /AWAKEN_RECORD_RELEASE_BINARY/);
  assert.match(harness, /CARGO_TARGET_DIR/);
  assert.match(harness, /'sandbox_tier = "namespace"'/);
  assert.match(harness, /data_dir = \$\{JSON\.stringify\(join\(root, "data"\)\)\}/);
  assert.match(harness, /spawn\(binary, \["all-in-one", "--config", config\]/);
  assert.match(harness, /beforePid = host\.pid/);
  assert.match(harness, /if \(beforePid === afterPid\)/);
  assert.match(harness, /restartAllInOne: async/);
  assert.match(harness, /owned all-in-one shutdown/);
  assert.match(harness, /rmSync\(root, \{ recursive: true, force: true \}\)/);
  assert.match(source, /const ARTIFACT_PATH = `\/mnt\/session\/outputs\/\$\{ARTIFACT_NAME\}`/);
  assert.match(source, /restartReceipt = await restartAllInOne\(\)/);
  assert.match(source, /restartReceipt\.after_pid\)\.not\.toBe\(restartReceipt\.before_pid\)/);
  assert.match(source, /restartReceipt\.generation\)\.toBe\(2\)/);
  assert.match(source, /details\.technical-id/);
  assert.match(source, /Technical ID\|技术 ID/);
  assert.match(source, /event\.id === acceptedEventId/);
  assert.match(source, /Call write exactly once now to create the continuity artifact/);
  assert.match(source, /Awaken will pause that tool call before execution so a person can review it/);
  assert.match(source, /Do not approve or bypass the pending write yourself/);
  assert.doesNotMatch(source, /Stop at every human approval boundary/);
  assert.match(source, /event\.input\?\.file_path === ARTIFACT_PATH/);
  assert.doesNotMatch(source, /event\.input\?\.path/);
  assert.match(source, /event\.id === write\.id && event\.evaluated_permission === "ask"/);
  assert.match(source, /agent\.tool_result/);
  assert.match(source, /user\.tool_confirmation/);
  assert.match(source, /\/v1\/files\/\$\{artifact\.id\}\/content/);
  assert.match(source, /locator\("\.session-detail-layout \.segmented"\)\.getByRole\("button", \{ name: \/Artifacts\|产物\//);
});

test("the recording batch fails before its first frame when clean-install prerequisites are missing", () => {
  const batch = readFileSync(resolve(here, "record-all.mjs"), "utf8");
  const readme = readFileSync(resolve(here, "README.md"), "utf8");
  assert.match(batch, /createRequire\(resolve\(repo, "e2e\/package\.json"\)\)/);
  assert.match(batch, /requireFromE2e\.resolve\("@anthropic-ai\/sdk"\)/);
  assert.match(batch, /npm ci --prefix e2e/);
  assert.match(batch, /AWAKEN_RECORD_RELEASE_BINARY/);
  assert.match(batch, /CARGO_TARGET_DIR/);
  assert.match(readme, /npm ci --prefix e2e/);
  assert.match(readme, /AWAKEN_RECORD_RELEASE_BINARY/);
});

test("every direct Session read in supplemental stories carries the Managed protocol version", () => {
  for (const name of [
    "10-skill-optimized-agent.mjs",
    "06-scheduled-operation.mjs",
    "14-session-control.mjs",
    "19-codex-acp-agent.mjs",
  ]) {
    const source = readFileSync(resolve(name === "06-scheduled-operation.mjs" ? flowsDir : proofsDir, name), "utf8");
    assert.match(source, /import \{[^\n]*MANAGED_HEADERS[^\n]*\} from "\.\.\/support\/betas\.mjs"/);
    const reads = source.match(/page\.request\.get\(`\$\{BACKEND\}\/v1\/sessions[^\n]+/g) ?? [];
    assert.ok(reads.length > 0, `${name}: expected direct Session proof`);
    for (const read of reads) assert.match(read, /headers: MANAGED_HEADERS/, `${name}: ${read}`);
  }
});

test("the Agent authoring video proves an unsaved Preview and exact reviewed publication", () => {
  const agent = readFileSync(resolve(flowsDir, "02-build-agent.mjs"), "utf8");
  assert.match(agent, /getByLabel\(\/\^Model\$\|\^模型\$\//);
  assert.match(agent, /getByPlaceholder\("Coding Assistant"\).*API compatibility reviewer/);
  assert.match(agent, /api-compatibility-/);
  assert.doesNotMatch(agent, /references workspace catalog/);
  assert.match(agent, /status\(\)\)\.toBe\(404\)/);
  assert.match(agent, /Start preview\|开始预览/);
  assert.match(agent, /getByLabel\(\/\^Message to agent\$/);
  assert.doesNotMatch(agent, /getByPlaceholder\(\/Ask the agent/);
  assert.match(agent, /previewDraft\.config\.multiagent \?\? null/);
  assert.match(agent, /locator\("\.pill"\).*AI SDK/);
  assert.match(agent, /button", \{ name: "AG-UI"/);
  assert.match(agent, /Message to agent over AG-UI/);
  assert.match(agent, /protocols#protocol-ag-ui/);
  assert.match(agent, /priorAssistantCount = await agUiAnswers\.count\(\)/);
  assert.match(agent, /toHaveCount\(priorAssistantCount \+ 1/);
  assert.doesNotMatch(agent, /const agUiAnswer = activeProtocolPreview[^\n]+\.last\(\)/);
  assert.match(agent, /RUN_STARTED/);
  assert.match(agent, /RUN_FINISHED/);
  assert.match(agent, /Version used by new Sessions\|新 Session 使用的版本/);
  assert.match(agent, /publication\.source_revision/);
  assert.match(agent, /the publication freezes the exact reviewed Agent revision/);
  assert.match(agent, /Reasoning effort\|推理强度/);
  assert.match(agent, /Tools & permissions\|工具与权限/);
  assert.match(agent, /Skills & MCP\|Skills 与 MCP/);
  assert.match(agent, /Context compaction strategy\|上下文压缩策略/);
  assert.match(agent, /previewDraft\.config\.inference/);
  assert.match(agent, /savedConfig\.compaction/);
  assert.doesNotMatch(agent, /Memory & resources|Memory 与资源/);
});

test("Agent authoring cases enter the current review-first publication flow", () => {
  for (const [directory, name] of [
    [flowsDir, "02-build-agent.mjs"],
    [proofsDir, "03-tools-permissions.mjs"],
    [proofsDir, "04-resources-transparency.mjs"],
    [proofsDir, "06-ai-state-machine.mjs"],
  ]) {
    const source = readFileSync(resolve(directory, name), "utf8");
    assert.match(source, /Review & publish\|审阅并发布/, `${name}: use the current review action`);
  }
});

test("model setup proves the single Provider Connection workflow", () => {
  const model = readFileSync(resolve(proofsDir, "01-connect-model.mjs"), "utf8");
  assert.match(model, /Provider connections/);
  assert.match(model, /Verify & import models/);
  assert.match(model, /connected model is immediately discoverable for Agent authoring/);
  assert.match(model, /\/v1\/config\/provider-connections/);
  assert.doesNotMatch(model, /\/v1\/config\/providers/);
  assert.doesNotMatch(model, /\/v1\/config\/endpoints/);
  assert.doesNotMatch(model, /\/v1\/config\/offerings/);
  assert.doesNotMatch(model, /goto\(["']\/w\/default\/credentials/);
  assert.doesNotMatch(model, /Workspace profile|Workspace-default/);
  assert.match(model, /New API key\|新 API Key/);
});

test("live recording uses a current DeepSeek offering and verifies exact publication eligibility", () => {
  const models = readFileSync(resolve(here, "support/models.mjs"), "utf8");
  assert.match(models, /deepseek-v4-flash/);
  assert.doesNotMatch(models, /deepseek-chat/);
  assert.match(models, /activeModels\.includes\(LIVE_MODEL_ID\)/);
});

test("the model-test UI owns one fixed marketing prompt with no competing automatic turn", () => {
  const surface = readFileSync(resolve(here, "../src/surfaces/models.tsx"), "utf8");
  const transcript = readFileSync(resolve(here, "../src/components/session/Transcript.tsx"), "utf8");
  const types = readFileSync(resolve(here, "../src/lib/api/types.ts"), "utf8");
  const flow = readFileSync(resolve(proofsDir, "01-connect-model.mjs"), "utf8");
  assert.match(surface, /text: "Reply with exactly: MODEL READY"/);
  assert.match(surface, /Do not add explanation/);
  assert.doesNotMatch(surface, /Reply with a short confirmation that this model connection is working/);
  assert.doesNotMatch(flow, /composer\.press\("Enter"\)/);
  assert.match(flow, /model-test-modal/);
  assert.doesNotMatch(surface, /fixedModel=/);
  assert.doesNotMatch(transcript, /modelOverride|fixedModel|model override|覆盖模型/);
  assert.match(types, /InboundEvent = BetaManagedAgentsEventParams/);
  assert.doesNotMatch(types, /export type InboundEvent = \{/);
});

test("repeatable Agent stories never call the unsupported draft DELETE route", () => {
  const support = readFileSync(resolve(here, "support/control-plane.mjs"), "utf8");
  assert.doesNotMatch(support, /request\.delete\(`\$\{BACKEND\}\/v1\/config\/agents/);
  for (const name of [
    "03-tools-permissions.mjs",
    "06-ai-state-machine.mjs",
  ]) {
    const source = readFileSync(resolve(proofsDir, name), "utf8");
    assert.doesNotMatch(source, /deleteAgentIfPresent/, `${name}: use a run-scoped Agent id`);
    assert.match(source, /Date\.now\(\)/, `${name}: create a fresh reviewable draft per recording`);
  }
});

test("frontend protocol proof uses a narrow Session-bound application token", () => {
  const source = readFileSync(resolve(proofsDir, "17-frontend-protocols.mjs"), "utf8");
  assert.match(source, /\/v1\/application-access-tokens/);
  assert.match(source, /protocols: \["ai-sdk", "ag-ui"\]/);
  assert.match(source, /operations: \["thread\.run", "thread\.messages\.read"\]/);
  assert.match(source, /external_thread_id: THREAD_ID, managed_session_id: session\.id/);
  assert.match(source, /authorization: `Bearer \$\{access\.access_token\}`/);
});

test("shared write-only secrets preserve an accessible label/control contract", () => {
  const source = readFileSync(resolve(here, "../src/components/ui/SecretField.tsx"), "utf8");
  assert.match(source, /<label htmlFor=\{inputId\}>\{label\}<\/label>/);
  assert.match(source, /id=\{inputId\}/);
  assert.match(source, /type="password"/);
});

test("recording copy and fixtures follow current Agent and Environment contracts", () => {
  const controlPlane = readFileSync(resolve(here, "support/control-plane.mjs"), "utf8");
  assert.match(controlPlane, /data: \{ environment_id: "env_local", \.\.\.data \}/);
  assert.match(controlPlane, /display_title: name/);
  assert.doesNotMatch(controlPlane, /\n\s+name,\n/);
  const assistant = readFileSync(resolve(proofsDir, "05-ai-authoring.mjs"), "utf8");
  assert.match(assistant, /Ask Assistant\|询问助手/);
  assert.match(assistant, /Ask a question or describe what you want to accomplish\|提问，或描述你想完成的事情/);
  assert.doesNotMatch(assistant, /Draft with AI\|用 AI 起草/);
  assert.doesNotMatch(assistant, /Describe the agent you want\|描述你想要的 agent/);
  const stateMachine = readFileSync(resolve(proofsDir, "06-ai-state-machine.mjs"), "utf8");
  assert.match(stateMachine, /putAgent\(page, AGENT_ID, DRAFT\)/);
  assert.match(stateMachine, /write "UNSAFE" to aha\.txt/);
  assert.match(stateMachine, /key: "\{file_path\}"/);
  assert.match(stateMachine, /write\(file_path ~ "\*"\)/);
  assert.doesNotMatch(stateMachine, /\/work\/aha\.txt/);
  assert.doesNotMatch(stateMachine, /Ask Assistant\|询问助手/);
  const skill = readFileSync(resolve(proofsDir, "10-skill-optimized-agent.mjs"), "utf8");
  assert.match(skill, /type: "agent_toolset_20260401"/);
  assert.match(skill, /name: "read", enabled: true/);
  assert.match(skill, /read its SKILL\.md with the read tool/);
  assert.match(skill, /event\.name === "read"/);
  assert.doesNotMatch(skill, /tools: \["list_skills", "Skill"\]/);
  assert.match(skill, /plugin_config: \{\}/);
  assert.doesNotMatch(skill, /plugin_config: \{ permission:/);
  const control = readFileSync(resolve(proofsDir, "14-session-control.mjs"), "utf8");
  assert.match(control, /expect\(composer\)\.toBeEnabled\(\{ timeout: 60_000 \}\)/);
  const deployment = readFileSync(resolve(flowsDir, "06-scheduled-operation.mjs"), "utf8");
  assert.match(deployment, /const ENVIRONMENT_ID = "env_local"/);
  assert.match(deployment, /selectOption\(ENVIRONMENT_ID\)/);
  assert.doesNotMatch(deployment, /createEnvironment|type: "cloud"/);
  assert.doesNotMatch(deployment, /config: \{[^\n]*runtime:/);
});

test("recording guidance separates Provider authentication from ACP credentials", () => {
  const readme = readFileSync(resolve(here, "README.md"), "utf8");
  assert.match(readme, /There is no\s+Workspace-default model step/);
  assert.match(readme, /claude setup-token/);
  assert.match(readme, /does not run an OAuth login inside ACP/);
  assert.match(readme, /Codex ACP executes the real Codex CLI through its persisted local login/);
});

test("recording guidance starts the canonical product launcher", () => {
  // Cause/effect decision table: C1=reader follows the recording guide or an
  // error prompt; C2=the CLI accepts only the canonical launcher name. R1
  // C1+C2+`all-in-one` reaches startup; R2 C1+deprecated `serve` is rejected
  // before recording. Every reader-facing path must therefore name the same
  // launcher and no retired alias.
  const readme = readFileSync(resolve(here, "README.md"), "utf8");
  const harness = readFileSync(resolve(here, "harness.mjs"), "utf8");
  const config = readFileSync(resolve(here, "recording-all-in-one.toml"), "utf8");
  assert.match(readme, /target\/release\/awaken all-in-one \\[\s\S]*?--config web\/record\/recording-all-in-one\.toml/);
  assert.match(config, /sandbox_tier = "namespace"/);
  assert.doesNotMatch(readme, /cargo run[^\n]*awaken-cli|target\/debug\/awaken/);
  assert.match(readme, /printed by\s+`awaken all-in-one`/);
  assert.match(harness, /printed by `awaken all-in-one`/);
  assert.doesNotMatch(`${readme}\n${harness}`, /`awaken serve`|-- serve(?:\s|$)/);
});

test("the UI audit can capture current release frames for visual review", () => {
  const audit = readFileSync(resolve(here, "ui-audit.mjs"), "utf8");
  assert.match(audit, /AWAKEN_UI_AUDIT_SCREENSHOTS/);
  assert.match(audit, /rmSync\(screenshotDir, \{ recursive: true, force: true \}\)/);
  assert.match(audit, /screenshotDir && name !== "tablet"/);
  // `.content` owns scrolling, so Playwright's document-level fullPage option
  // cannot reach the lower half of long routes. Require both viewport edges.
  assert.match(audit, /content\.scrollTop = content\.scrollHeight/);
  assert.match(audit, /-bottom\.png/);
  assert.doesNotMatch(audit, /fullPage: true/);
  assert.match(audit, /document\.querySelectorAll\("main \.skeleton"\)\.length === 0/);
  assert.match(audit, /layout\.skeletons/);
  assert.match(audit, /layout\.loadingCells/);
  assert.match(audit, /Skill editor did not load the official content archive/);
  assert.match(audit, /file\.filename === "release-readiness\.md"/);
  assert.match(audit, /matchingFiles\.slice\(1\)/);
  assert.match(audit, /AWAKEN_UI_AUDIT_IDENTITY_MODE/);
  assert.match(audit, /navigation-write-side-effect/);
  assert.match(audit, /expectedSessionIds/);
  assert.match(audit, /candidate\.title === "UI audit session"/);
  assert.match(audit, /candidate\.agent\?\.id === agentId/);
  assert.doesNotMatch(audit, /body\.data\?\.\[0\]\?\.id/);
});

test("marketing stories and proof-only tests together cover release-ready capabilities", () => {
  assert.deepEqual(flows, MARKETING_STORIES.map((slug) => `${slug}.mjs`));
  assert.equal(TARGET_MARKETING_STORIES.length, 6);
  assert.deepEqual(TARGET_MARKETING_STORIES.slice(0, 2), MARKETING_STORIES.slice(0, 2));
  assert.ok(TARGET_MARKETING_STORIES.includes("06-scheduled-operation"));
  const proofs = readdirSync(proofsDir).filter((name) => name.endsWith(".mjs")).sort();
  assert.deepEqual(proofs, PRODUCT_PROOFS.map((slug) => `${slug}.mjs`));
  const corpus = [
    ...flows.map((name) => readFileSync(resolve(flowsDir, name), "utf8")),
    ...proofs.map((name) => readFileSync(resolve(proofsDir, name), "utf8")),
  ].join("\n");
  for (const claim of ["Skill", "resource", "Deployment", "/v1/sessions", "archive", "A2A", "revoke", "AI SDK", "AG-UI"]) {
    assert.ok(corpus.includes(claim), `series: missing supplemental proof for ${claim}`);
  }
});

test("proof-only cases cannot leak back into recording or publication", () => {
  assert.deepEqual(MARKETING_STORIES.filter((slug) => PRODUCT_PROOFS.includes(slug)), []);
  const recorder = readFileSync(resolve(here, "record-all.mjs"), "utf8");
  const manifest = readFileSync(resolve(here, "release-manifest.mjs"), "utf8");
  const proofHarness = readFileSync(resolve(here, "proof-harness.mjs"), "utf8");
  const proofRunner = readFileSync(resolve(here, "run-proofs.mjs"), "utf8");
  assert.match(recorder, /const slugs = requested\.length > 0 \? requested : MARKETING_STORIES/);
  assert.match(recorder, /unknown marketing story/);
  assert.match(manifest, /recorded: false/);
  assert.doesNotMatch(proofHarness, /recordVideo|ffmpeg|\.mp4/);
  assert.match(proofRunner, /timeout: 210_000/);
  for (const slug of PRODUCT_PROOFS) {
    const source = readFileSync(resolve(proofsDir, `${slug}.mjs`), "utf8");
    assert.doesNotMatch(source, /export const story/, `${slug}: proof still declares a publishable story`);
  }
});

test("the release set excludes configuration-only stories without an effect", () => {
  assert.ok(!flows.includes("08-agent-control-plane.mjs"));
  assert.ok(!flows.includes("09-protocol-composition.mjs"));
  const strategy = readFileSync(resolve(here, "VIDEO_STRATEGY.md"), "utf8");
  assert.match(strategy, /A proof returns to the public series only when it gains all five elements/);
  assert.match(strategy, /protocol handshakes, denial tests, or infrastructure receipts as marketing content/);
});

test("the Codex ACP proof executes a real adapter result rather than recording configuration", () => {
  const source = readFileSync(resolve(proofsDir, "19-codex-acp-agent.mjs"), "utf8");
  const models = readFileSync(resolve(here, "support/models.mjs"), "utf8");
  assert.match(source, /AWAKEN_RECORD_CODEX_ACP/);
  assert.match(source, /AWAKEN_RECORD_CODEX_PERSISTED_LOGIN/);
  assert.match(source, /Codex ACP capability readback/);
  assert.match(source, /`\$\{BACKEND\}\/v1\/capabilities`/);
  assert.doesNotMatch(source, /`\$\{BACKEND\}\/v1\/config\/capabilities`/);
  assert.match(source, /codex\.local\.login_state !== "available" \|\| !codex\.local\.negotiated/);
  assert.match(source, /codex\.local\.login_state === "available" && codex\.local\.negotiated/);
  assert.match(source, /const deadline = Date\.now\(\) \+ 80_000/);
  assert.match(source, /await new Promise\(\(resolve\) => setTimeout\(resolve, 2_000\)\)/);
  assert.match(source, /acp:codex/);
  assert.match(source, /mode: "backend_exact", backend_ref: "acp:codex", model_ref: MODEL_ID/);
  assert.doesNotMatch(source, /acp:codex@openai/);
  assert.match(source, /executor=acp:codex/);
  assert.match(source, /agent-working/);
  assert.match(source, /CODEX ACP READY/);
  assert.doesNotMatch(source, /AWAKEN_ACP_CREDENTIAL_FILE/);
  assert.doesNotMatch(source, /auth\.json/);
  assert.doesNotMatch(source, /OPENAI_API_KEY/);
  assert.match(source, /answers\)\.toHaveLength\(1\)/);
  assert.doesNotMatch(source, /spawnSync/);
  assert.doesNotMatch(source, /configureSyntheticResponsesModel/);
  assert.match(models, /AWAKEN_RECORD_SYNTHETIC_HOST/);
});

test("dependency-gated stories fail honestly before making a product claim", () => {
  const a2a = readFileSync(resolve(proofsDir, "15-a2a-discovery.mjs"), "utf8");
  const access = readFileSync(resolve(proofsDir, "16-access-boundary.mjs"), "utf8");
  const mcp = readFileSync(resolve(proofsDir, "18-mcp-server-export.mjs"), "utf8");
  assert.match(a2a, /\.well-known\/agent-card\.json/);
  assert.doesNotMatch(a2a, /A2A_DELEGATE_ID|\/v1\/delegates\//);
  assert.match(access, /AWAKEN_RECORD_SETUP_TOKEN/);
  assert.match(access, /self-managed all-in-one host/);
  assert.match(mcp, /AWAKEN_RECORD_MCP_TOKEN/);
  assert.match(mcp, /mcp_bearer_token/);
  assert.doesNotMatch(mcp, /AWAKEN_MCP_BEARER_TOKEN/);
  const readme = readFileSync(resolve(here, "README.md"), "utf8");
  assert.match(readme, /MCP export is fail-closed/);
  assert.match(readme, /does not discover\s+deployment credentials from `AWAKEN_MCP_BEARER_TOKEN`/);
  assert.match(readme, /never commit it/);
});
