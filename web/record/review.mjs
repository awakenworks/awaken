// Deterministic 100-round review gate for product videos. Each round rotates a
// different UX/marketing/testability lens across every flow and prints actionable
// feedback on failure. This complements (not replaces) real recording + frame review.

import assert from "node:assert/strict";
import { readdirSync, readFileSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const flowDir = resolve(here, "flows");
const flowNames = readdirSync(flowDir).filter((name) => name.endsWith(".mjs")).sort();
const flows = new Map(flowNames.map((name) => [name, readFileSync(resolve(flowDir, name), "utf8")]));
const harness = readFileSync(resolve(here, "harness.mjs"), "utf8");
const readme = readFileSync(resolve(here, "README.md"), "utf8");

const checks = [
  ["story order", () => eachFlow((name, source) => {
    const intro = source.indexOf("await intro(");
    const firstOperation = firstIndex(source, ["await click(", "await type(", "await say("]);
    assert.ok(intro >= 0 && intro < firstOperation, `${name}: explain intent/function before UI operations`);
  })],
  ["executable claim", () => eachFlow((name, source) => {
    assert.match(source, /await (?:checkpoint|runtimeCheckpoint)\(/, `${name}: add an observable checkpoint`);
    assert.match(source, /expect\(|waitForFunction\(/, `${name}: checkpoint must assert UI/API truth`);
  })],
  ["shareable payoff", () => eachFlow((name, source) => {
    assert.equal(matches(source, /await aha\(/g), 1, `${name}: land exactly one focused AHA`);
    const proof = Math.max(source.lastIndexOf("await checkpoint("), source.lastIndexOf("await runtimeCheckpoint("));
    assert.ok(source.lastIndexOf("await aha(") > proof, `${name}: AHA must follow proof`);
  })],
  ["no swallowed interaction", () => eachFlow((name, source) => {
    assert.doesNotMatch(source, /(?:click|fill|selectOption|waitFor)[^;\n]*\.catch\(/, `${name}: visible interaction failure is being swallowed`);
  })],
  ["intent and function copy", () => eachFlow((name, source) => {
    const intro = source.match(/await intro\(\s*"([^"]+)",\s*"([^"]+)"/s);
    assert.ok(intro, `${name}: intro needs literal intent and capability copy`);
    assert.ok(intro[1].length >= 35 && intro[2].length >= 35, `${name}: intro is too vague`);
  })],
  ["caption readability", () => eachFlow((name, source) => {
    for (const text of literalCallArgs(source, ["say", "aha"])) {
      assert.ok([...text].length <= 150, `${name}: caption exceeds 150 characters: ${text}`);
    }
  })],
  ["brand close", () => {
    assert.match(harness, /awaken · configure, prove, and run agents/);
    assert.match(harness, /AHA ·/);
  }],
  ["subtitle deliverables", () => {
    for (const extension of ["captions.json", ".vtt", ".srt"]) assert.ok(harness.includes(extension), `harness: missing ${extension}`);
    assert.match(harness, /readingMs/);
  }],
  ["visible verification", () => {
    assert.match(harness, /Live verification/);
    assert.match(harness, /✓ Verified/);
    assert.match(harness, /rec-proof/);
  }],
  ["failure artifact", () => {
    assert.match(harness, /\.failed/);
    assert.match(readme, /failed assertion produces only/);
  }],
  ["no turn vocabulary", () => eachFlow((name, source) => {
    assert.doesNotMatch(source, /\bturns?\b/i, `${name}: use run/step/thread vocabulary`);
  })],
  ["real product surfaces", () => eachFlow((name, source) => {
    assert.match(source, /page\.|goto\(/, `${name}: no real product surface is driven`);
    assert.doesNotMatch(source, /mock|fake response/i, `${name}: scripted model result presented as real`);
  })],
  ["state runtime proof", () => {
    const source = requiredFlow("06-ai-state-machine.mjs");
    assert.match(source, /Try it\|试运行/);
    assert.match(source, /State Machine blocks the unread write at runtime/);
    assert.match(source, /Agent working\|Agent 工作中/);
  }],
  ["repeat-safe state", () => {
    const source = requiredFlow("06-ai-state-machine.mjs");
    assert.match(source, /\[read, written\]/);
    assert.match(source, /arrayContaining\(\["read", "written"\]\)/);
  }],
  ["runtime tool identity", () => {
    assert.match(requiredFlow("03-tools-permissions.mjs"), /bash\(command/);
    assert.doesNotMatch(requiredFlow("06-ai-state-machine.mjs"), /\bRead\(|\bWrite\(/);
  }],
  ["credential safety", () => {
    const source = requiredFlow("01-connect-model.mjs");
    assert.match(source, /type="password"|getByLabel\(\/Secret/i);
    assert.doesNotMatch(source, /console\.log\([^\n]*KEY/);
  }],
  ["feature breadth", () => {
    const corpus = [...flows.values()].join("\n");
    for (const claim of [
      "model", "agent", "permission", "memory", "State Machine", "trace",
      "Managed Agents", "ACP", "MCP", "sandbox", "Skill", "Deployment",
      "archive", "A2A", "access",
    ]) {
      assert.ok(corpus.toLowerCase().includes(claim.toLowerCase()), `series: missing ${claim}`);
    }
  }],
  ["agent control proof", () => {
    const source = requiredFlow("08-agent-control-plane.mjs");
    for (const claim of ["context_policy", "compact.instructions", "memory.instructions", "memory.extraction_prompt", "continuation"]) {
      assert.ok(source.includes(claim), `08-agent-control-plane.mjs: missing ${claim} checkpoint`);
    }
  }],
  ["protocol composition proof", () => {
    const source = requiredFlow("09-protocol-composition.mjs");
    for (const claim of ["Managed Agents", "acp:claude", "credential_binding", "Project-bound MCP servers merge"]) {
      assert.ok(source.includes(claim), `09-protocol-composition.mjs: missing ${claim}`);
    }
  }],
  ["interaction pacing", () => eachFlow((name, source) => {
    assert.match(source, /await wait\(/, `${name}: add a visual settle after interaction`);
  })],
  ["caption-scene coupling", () => {
    const overview = requiredFlow("00-platform-overview.mjs");
    assert.ok(matches(overview, /await beat\(/g) >= 7, "overview: every capability beat should focus its visible evidence");
    assert.match(harness, /\.rec-focus/);
  }],
  ["no static pauses", () => {
    assert.match(harness, /Math\.min\(5200,/);
    eachFlow((name, source) => {
      for (const match of source.matchAll(/await wait\(([0-9_]+)\)/g)) {
        const milliseconds = Number(match[1].replaceAll("_", ""));
        assert.ok(milliseconds <= 2500, `${name}: unexplained static wait is ${milliseconds}ms`);
      }
    });
  }],
  ["responsive Agent feedback", () => {
    const authoring = requiredFlow("02-build-agent.mjs");
    assert.match(authoring, /Shift\+Enter/);
    assert.match(authoring, /transcript-pending-message/);
    assert.match(authoring, /agent-working/);
  }],
  ["MCP override proof", () => {
    const tools = requiredFlow("03-tools-permissions.mjs");
    assert.match(tools, /mcp__issues__create_issue/);
    assert.match(tools, /config\.tools\)\.not\.toContain/);
    assert.match(tools, /config\.mcp_servers/);
  }],
  ["Memory effect proof", () => {
    const memory = requiredFlow("04-resources-transparency.mjs");
    assert.ok(matches(memory, /\/v1\/sessions/g) >= 2, "memory: use two fresh sessions");
    assert.match(memory, /persisted\.content/);
    assert.match(memory, /getByText\(secret/);
  }],
  ["recording test contract", () => {
    assert.match(readme, /intro\(intent, capability\)/);
    assert.match(readme, /checkpoint\(name, assertion\)/);
    assert.match(readme, /aha\(text\)/);
  }],
  ["series completeness", () => {
    assert.deepEqual(flowNames, [
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
    ]);
  }],
  ["customer relationship objective", () => eachFlow((name, source) => {
    for (const field of ["loyalty", "satisfaction", "advocacy"]) {
      assert.match(source, new RegExp(`${field}:\\s*["']`), `${name}: explain how this story strengthens ${field}`);
    }
  })],
  ["sub-three-minute close", () => {
    assert.match(harness, /MAX_VIDEO_MS = 180_000/);
    assert.match(harness, /MAX_FLOW_MS = 172_000/);
  }],
];

for (let round = 1; round <= 100; round += 1) {
  const [name, check] = checks[(round - 1) % checks.length];
  try {
    check();
    console.log(`[review ${String(round).padStart(3, "0")}/100] PASS · ${name}`);
  } catch (error) {
    console.error(`[review ${String(round).padStart(3, "0")}/100] FIX · ${name} · ${error.message}`);
    process.exitCode = 1;
    break;
  }
}

if (!process.exitCode) console.log("[review] 100/100 UX and product-claim rounds passed");

function eachFlow(check) {
  for (const [name, source] of flows) check(name, source);
}

function requiredFlow(name) {
  const source = flows.get(name);
  assert.ok(source, `missing flow ${name}`);
  return source;
}

function matches(source, pattern) {
  return [...source.matchAll(pattern)].length;
}

function firstIndex(source, needles) {
  return Math.min(...needles.map((needle) => {
    const index = source.indexOf(needle);
    return index < 0 ? Number.POSITIVE_INFINITY : index;
  }));
}

function literalCallArgs(source, names) {
  const pattern = new RegExp(`await (?:${names.join("|")})\\(\\s*([\"'\\\`])([\\s\\S]*?)\\1`, "g");
  return [...source.matchAll(pattern)].map((match) => match[2]);
}
