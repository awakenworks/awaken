import assert from "node:assert/strict";
import { readdirSync, readFileSync } from "node:fs";
import test from "node:test";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const flowsDir = resolve(here, "flows");
const flows = readdirSync(flowsDir).filter((name) => name.endsWith(".mjs")).sort();

test("every recording is a story-shaped executable test", () => {
  assert.ok(flows.length > 0, "at least one recording flow exists");
  for (const name of flows) {
    const source = readFileSync(resolve(flowsDir, name), "utf8");
    assert.match(source, /await intro\(/, `${name}: explain intent and capability before operating`);
    assert.match(source, /await (?:checkpoint|runtimeCheckpoint)\(/, `${name}: assert at least one product claim`);
    assert.match(source, /await aha\(/, `${name}: land a visible, shareable payoff`);
    assert.match(source, /export const story\s*=\s*{/, `${name}: declare one customer story`);
    for (const field of ["promise", "effect", "aha", "loyalty", "satisfaction", "advocacy"]) {
      assert.match(source, new RegExp(`${field}:\\s*["']`), `${name}: story.${field} is required`);
    }
  }
});

test("every recording has a hard sub-three-minute budget", () => {
  const harness = readFileSync(resolve(here, "harness.mjs"), "utf8");
  assert.match(harness, /MAX_VIDEO_MS = 180_000/);
  assert.match(harness, /MAX_FLOW_MS = 172_000/);
  assert.match(harness, /expected exactly one AHA/);
  assert.match(harness, /runtime failed before the claimed effect/);
});

// Recording FMECA cause/effect graph:
// C1=current flow passes, C2=FFmpeg succeeds, C3=a prior artifact exists,
// C4=focused evidence is low in the viewport, C5=the recorded page exists.
// E1=one atomic current MP4, E2=non-zero + diagnostic WebM, E3=no stale success,
// E4=caption/proof move away from evidence, E5=captions share the video epoch.
// Decision rules: 11***→E1; 10***→E2; **1**→E3; ***1*→E4; ****1→E5.
test("recording publication fails closed and cannot reuse a stale success", () => {
  const harness = readFileSync(resolve(here, "harness.mjs"), "utf8");
  assert.match(harness, /for \(const artifact of \[slug, `\$\{slug\}\.failed`\]\)/);
  assert.match(harness, /\.mp4\.tmp\.mp4/);
  assert.match(harness, /renameSync\(temporaryMp4, mp4\)/);
  assert.match(harness, /ffmpeg failed:/);
  assert.match(harness, /failed = true;/);
  assert.match(harness, /failure: failureReason/);
});

test("recording chrome shares the video timeline and avoids focused evidence", () => {
  const harness = readFileSync(resolve(here, "harness.mjs"), "utf8");
  assert.match(harness, /const SIZE = \{ width: 1600, height: 900 \}/);
  assert.match(harness, /const videoStartedAt = Date\.now\(\)/);
  assert.match(harness, /startMs = Date\.now\(\) - videoStartedAt/);
  assert.match(harness, /classList\.toggle\("top", targetIsLow\)/);
  assert.match(harness, /classList\.toggle\("low", targetIsLow\)/);
  assert.match(harness, /Recording stopped/);
});

test("the recording browser uses the one-time local setup handoff, not a service credential", () => {
  const harness = readFileSync(resolve(here, "harness.mjs"), "utf8");
  const models = readFileSync(resolve(here, "support/models.mjs"), "utf8");
  assert.match(harness, /AWAKEN_RECORD_SETUP_TOKEN/);
  assert.match(harness, /\/v1\/auth\/local\/exchange/);
  assert.match(harness, /HttpOnly browser/);
  assert.doesNotMatch(harness, /admin-token/);
  assert.match(models, /\/v1\/config\/workspace-context/);
  assert.doesNotMatch(models, /wrkspc_default/);
});

test("the state-machine recording proves runtime enforcement, not just configuration", () => {
  const source = readFileSync(resolve(flowsDir, "06-ai-state-machine.mjs"), "utf8");
  assert.match(source, /Try draft\|试运行草稿/);
  assert.match(source, /Start preview\|开始预览/);
  assert.match(source, /State Machine blocks the unread write at runtime/);
  assert.match(source, /getByText\("error"/);
  assert.ok(source.includes("getByText(/blocked|Read .* before writing/i)"));
});

test("interactive transcripts follow committed SSE frames without a manual refresh", () => {
  const transcript = readFileSync(resolve(here, "../src/components/session/Transcript.tsx"), "utf8");
  const hook = readFileSync(resolve(here, "../src/lib/useSessionLog.ts"), "utf8");
  assert.match(transcript, /useSessionLog\(base, queryKey, live, composer\)/);
  assert.match(hook, /if \(followLive\)/);
  assert.match(hook, /setQueryData<SessionEvent\[\]>/);
});

test("overview captions point to visible UI evidence without long static waits", () => {
  const overview = readFileSync(resolve(flowsDir, "00-platform-overview.mjs"), "utf8");
  const harness = readFileSync(resolve(here, "harness.mjs"), "utf8");
  assert.ok([...overview.matchAll(/await beat\(/g)].length >= 7);
  assert.match(harness, /\.rec-focus/);
  assert.match(harness, /Math\.min\(5200,/);
});

test("MCP and Memory videos prove runtime-relevant effects", () => {
  const tools = readFileSync(resolve(flowsDir, "03-tools-permissions.mjs"), "utf8");
  const memory = readFileSync(resolve(flowsDir, "04-resources-transparency.mjs"), "utf8");
  assert.match(tools, /mcp__issues__create_issue/);
  assert.match(tools, /config\.tools\)\.not\.toContain/);
  assert.ok([...memory.matchAll(/\/v1\/sessions/g)].length >= 2);
  assert.match(memory, /persisted\.content/);
  assert.match(memory, /getByText\(secret/);
  assert.doesNotMatch(memory, /Save resources|保存资源/);
  assert.match(memory, /publication\.agent_inputs\.inputs/);
  assert.match(memory, /MEMORY_HEADERS/);
  assert.match(memory, /MANAGED_HEADERS/);
});

test("the live-model connection video asserts agent output, never page copy", () => {
  const model = readFileSync(resolve(flowsDir, "01-connect-model.mjs"), "utf8");
  assert.match(model, /locator\("\.card"\)\.filter\(\{ hasText: "⬡ agent" \}\)/);
  assert.doesNotMatch(model, /document\.body\.innerText/);
  assert.match(model, /expect\(agentReply\)\.toContainText\("MODEL READY"/);
});

test("the Agent authoring video proves unsaved Resource Preview and an exact combined publication", () => {
  const agent = readFileSync(resolve(flowsDir, "02-build-agent.mjs"), "utf8");
  assert.match(agent, /status\(\)\)\.toBe\(404\)/);
  assert.match(agent, /Start preview\|开始预览/);
  assert.match(agent, /Publication snapshot\|发布快照/);
  assert.match(agent, /publication\.source_revision/);
  assert.match(agent, /publication\.agent_inputs\.inputs/);
  assert.match(agent, /MEMORY_HEADERS/);
  assert.doesNotMatch(agent, /Save resources|保存资源/);
});

test("model setup records the single Provider Connection workflow", () => {
  const model = readFileSync(resolve(flowsDir, "01-connect-model.mjs"), "utf8");
  assert.match(model, /Provider connections/);
  assert.match(model, /Verify & import models/);
  assert.match(model, /immediately available to Agent pickers and the Assistant/);
  assert.match(model, /\/v1\/config\/provider-connections/);
  assert.doesNotMatch(model, /\/v1\/config\/providers/);
  assert.doesNotMatch(model, /\/v1\/config\/endpoints/);
  assert.doesNotMatch(model, /\/v1\/config\/offerings/);
  assert.doesNotMatch(model, /goto\(["']\/w\/default\/credentials/);
  assert.doesNotMatch(model, /Workspace profile|Workspace-default/);
});

test("recording guidance separates Provider authentication from ACP credentials", () => {
  const readme = readFileSync(resolve(here, "README.md"), "utf8");
  assert.match(readme, /There is no\s+Workspace-default model step/);
  assert.match(readme, /claude setup-token/);
  assert.match(readme, /does not run an OAuth login inside ACP/);
  assert.match(readme, /Codex ACP uses its native operator-selected `auth\.json`/);
});

test("the complete series covers every release-ready platform capability", () => {
  assert.deepEqual(flows, [
    "00-platform-overview.mjs",
    "01-connect-model.mjs",
    "02-build-agent.mjs",
    "03-tools-permissions.mjs",
    "04-resources-transparency.mjs",
    "05-ai-authoring.mjs",
    "06-ai-state-machine.mjs",
    "10-skill-optimized-agent.mjs",
    "11-resource-provenance.mjs",
    "12-deployment-control.mjs",
    "13-managed-api-ingress.mjs",
    "14-session-control.mjs",
    "15-a2a-discovery.mjs",
    "16-access-boundary.mjs",
    "17-frontend-protocols.mjs",
    "18-mcp-server-export.mjs",
    "19-codex-acp-agent.mjs",
  ]);
  const corpus = flows.map((name) => readFileSync(resolve(flowsDir, name), "utf8")).join("\n");
  for (const claim of ["Skill", "resource", "Deployment", "/v1/sessions", "archive", "A2A", "revoke", "AI SDK", "AG-UI"]) {
    assert.ok(corpus.includes(claim), `series: missing supplemental proof for ${claim}`);
  }
});

test("the release set excludes configuration-only stories without an effect", () => {
  assert.ok(!flows.includes("08-agent-control-plane.mjs"));
  assert.ok(!flows.includes("09-protocol-composition.mjs"));
  const strategy = readFileSync(resolve(here, "VIDEO_STRATEGY.md"), "utf8");
  assert.match(strategy, /Agent behavior controls are proven by the Memory and State Machine runtime stories/);
  assert.match(strategy, /Protocol composition is proven by the\s+runtime MCP and Codex ACP stories/);
});

test("the Codex ACP video proves a real adapter result rather than configuration", () => {
  const source = readFileSync(resolve(flowsDir, "19-codex-acp-agent.mjs"), "utf8");
  assert.match(source, /AWAKEN_RECORD_CODEX_ACP/);
  assert.match(source, /AWAKEN_RECORD_CODEX_ACP_CONTAINER/);
  assert.match(source, /label=awaken\.sandbox=1/);
  assert.match(source, /acp:codex/);
  assert.match(source, /agent-working/);
  assert.match(source, /CODEX ACP READY/);
  assert.match(source, /AWAKEN_ACP_CREDENTIAL_FILE/);
  assert.match(source, /\/acp-config\/auth\.json/);
  assert.match(source, /apiKeyAbsent/);
  assert.match(source, /writableCredential/);
  assert.match(source, /proof\.user === "10001"/);
  assert.match(source, /answers\)\.toHaveLength\(1\)/);
});

test("dependency-gated stories fail honestly before making a product claim", () => {
  const a2a = readFileSync(resolve(flowsDir, "15-a2a-discovery.mjs"), "utf8");
  const access = readFileSync(resolve(flowsDir, "16-access-boundary.mjs"), "utf8");
  const mcp = readFileSync(resolve(flowsDir, "18-mcp-server-export.mjs"), "utf8");
  assert.match(a2a, /A2A_DELEGATE_ID/);
  assert.match(access, /AWAKEN_RECORD_ADMIN_TOKEN/);
  assert.match(access, /embedded-IAM host/);
  assert.match(mcp, /AWAKEN_RECORD_MCP_TOKEN/);
  assert.match(mcp, /AWAKEN_MCP_BEARER_TOKEN/);
});
