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

test("the state-machine recording proves runtime enforcement, not just configuration", () => {
  const source = readFileSync(resolve(flowsDir, "06-ai-state-machine.mjs"), "utf8");
  assert.match(source, /Try it\|试运行/);
  assert.match(source, /Start session\|开始会话/);
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
});

test("live-model videos assert agent output, never the user's prompt or page copy", () => {
  const model = readFileSync(resolve(flowsDir, "01-connect-model.mjs"), "utf8");
  const agent = readFileSync(resolve(flowsDir, "02-build-agent.mjs"), "utf8");
  for (const source of [model, agent]) {
    assert.match(source, /locator\("\.card"\)\.filter\(\{ hasText: "⬡ agent" \}\)/);
    assert.doesNotMatch(source, /document\.body\.innerText/);
  }
  assert.match(model, /expect\(agentReply\)\.toContainText\("MODEL READY"/);
  assert.match(agent, /expect\(agentReply\)\.toContainText\(\/Features\|Fixes\|Breaking changes\/i/);
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
    "07-runtime-sandbox.mjs",
    "08-agent-control-plane.mjs",
    "09-protocol-composition.mjs",
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
