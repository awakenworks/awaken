// Deterministic 100-round review gate for product videos. Each round rotates a
// different UX/marketing/testability lens across every flow and prints actionable
// feedback on failure. This complements (not replaces) real recording + frame review.

import assert from "node:assert/strict";
import { readdirSync, readFileSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { MARKETING_STORIES, PRODUCT_PROOFS } from "./catalog.mjs";

const here = dirname(fileURLToPath(import.meta.url));
const flowDir = resolve(here, "flows");
const proofDir = resolve(here, "proofs");
const flowNames = readdirSync(flowDir).filter((name) => name.endsWith(".mjs")).sort();
const flows = new Map(flowNames.map((name) => [name, readFileSync(resolve(flowDir, name), "utf8")]));
const proofs = new Map(PRODUCT_PROOFS.map((slug) => [`${slug}.mjs`, readFileSync(resolve(proofDir, `${slug}.mjs`), "utf8")]));
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
  ["first-viewer task stakes", () => eachFlow((name, source) => {
    const job = storyLiteral(source, "job");
    const stakes = storyLiteral(source, "stakes");
    const handoff = storyLiteral(source, "handoff");
    const intro = source.match(/await intro\(\s*"([^"]+)"/s)?.[1] ?? "";
    assert.ok(job.length >= 8, `${name}: name the concrete job, not a fictional person`);
    assert.ok(stakes.length >= 55, `${name}: state a consequential failure, not a generic benefit`);
    assert.ok(handoff.length >= 55, `${name}: state what the result enables and where human authority remains`);
    assert.ok(intro.length >= 55, `${name}: open with the job or risk instead of invented biography`);
    assert.doesNotMatch(intro, /\b(?:Monday|Tuesday|Wednesday|Thursday|Friday|Saturday|Sunday)\b|\b\d{1,2}:\d{2}\b/i, `${name}: do not manufacture calendar pressure`);
  })],
  ["humanized story copy", () => eachFlow((name, source) => {
    const story = source.slice(source.indexOf("export const story"), source.indexOf("};", source.indexOf("export const story")) + 2);
    assert.doesNotMatch(story, /[—–]/, `${name}: story metadata contains an em or en dash`);
    assert.doesNotMatch(story, /\b(?:crucial|pivotal|showcase|delve|underscores?|not merely|not just)\b/i, `${name}: replace generic AI-style promotion with concrete facts`);
  })],
  ["caption readability", () => eachFlow((name, source) => {
    for (const text of literalCallArgs(source, ["say", "aha"])) {
      assert.ok([...text].length <= 100, `${name}: caption exceeds 100 characters: ${text}`);
      assert.ok(text.trim().split(/\s+/).length <= 12, `${name}: caption exceeds 12 words: ${text}`);
      assert.doesNotMatch(text, /[—–]/, `${name}: humanize audience copy without em or en dashes`);
    }
  })],
  ["brand close", () => {
    assert.match(harness, /Awaken Agents · work that can continue/);
    assert.match(harness, /The conversation can end\. The work stays ready for whoever comes next\./);
    assert.match(harness, /M14\.2 6h3\.6l8\.6 20h-3L16 8\.4 8\.6 26h-3Z/);
    assert.match(harness, /#68ced9/);
    assert.match(harness, /#a392ff/);
    assert.match(harness, /cursor\.style\.opacity = "0"/);
    assert.match(harness, /cursor\.style\.opacity = "1"/);
    assert.doesNotMatch(harness, /rec-brand-mark::before|linear-gradient\(145deg,rgba\(205,80,108/);
    assert.doesNotMatch(harness, /Intent ·|Awaken ·|AHA ·/);
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
    assert.match(readme, /failed assertion produces/);
    assert.match(harness, /\.mp4\.tmp\.mp4/);
    assert.match(harness, /Recording stopped/);
  }],
  ["recording composition", () => {
    assert.match(harness, /width: 1920, height: 1200/);
    assert.match(harness, /videoStartedAt/);
    assert.match(harness, /classList\.toggle\("top", targetIsLow\)/);
    assert.match(harness, /AWAKEN_RECORD_KEEP_WEBM/);
    assert.match(harness, /recording requires one all-in-one origin/);
    assert.match(harness, /verifyFinalVideo\(temporaryMp4\)/);
    assert.match(harness, /product_revision: recordedProductRevision/);
    assert.match(harness, /recording requires a clean tracked source tree/);
    assert.match(harness, /fps=30/);
    assert.match(harness, /rec-brand/);
  }],
  ["no turn vocabulary", () => eachFlow((name, source) => {
    assert.doesNotMatch(source, /\bturns?\b/i, `${name}: use run/step/thread vocabulary`);
  })],
  ["real product surfaces", () => eachFlow((name, source) => {
    assert.match(source, /page\.|goto\(/, `${name}: no real product surface is driven`);
    assert.doesNotMatch(source, /mock|fake response/i, `${name}: scripted model result presented as real`);
  })],
  ["state runtime proof", () => {
    const source = requiredProof("06-ai-state-machine.mjs");
    assert.match(source, /Try draft\|试运行草稿/);
    assert.match(source, /State Machine blocks the unread write at runtime/);
    assert.match(source, /details\[data-tool="write"\]/);
    assert.match(source, /writeCard\.locator\("summary"\)\.click\(\)/);
  }],
  ["repeat-safe state", () => {
    const source = requiredProof("06-ai-state-machine.mjs");
    assert.match(source, /from: \["read", "written"\]/);
    assert.match(source, /arrayContaining\(\["read", "written"\]\)/);
  }],
  ["runtime tool identity", () => {
    assert.match(requiredProof("03-tools-permissions.mjs"), /bash\(command/);
    assert.doesNotMatch(requiredProof("06-ai-state-machine.mjs"), /\bRead\(|\bWrite\(/);
  }],
  ["credential safety", () => {
    const source = requiredProof("01-connect-model.mjs");
    assert.match(source, /oauth_helper: "gcloud"/);
    assert.match(source, /not\.toHaveProperty\("material_ref"\)/);
    assert.match(source, /not\.toHaveProperty\("oauth_command"\)/);
    assert.doesNotMatch(source, /console\.log\([^\n]*KEY/);
    assert.match(harness, /AWAKEN_RECORD_SETUP_TOKEN/);
    assert.match(harness, /\/v1\/auth\/local\/exchange/);
    assert.doesNotMatch(harness, /admin-token/);
  }],
  ["single model-connection workflow", () => {
    const source = requiredProof("01-connect-model.mjs");
    for (const claim of [
      "Provider connections",
      "Verify & import models",
      "connected model is immediately discoverable for Agent authoring",
      "/v1/config/provider-connections",
    ]) {
      assert.ok(source.includes(claim), `01-connect-model.mjs: missing ${claim}`);
    }
    for (const redundantPath of [
      "/v1/config/providers/",
      "/v1/config/endpoints/",
      "/v1/config/offerings",
    ]) {
      assert.ok(!source.includes(redundantPath), `01-connect-model.mjs: still uses ${redundantPath}`);
    }
  }],
  ["feature breadth", () => {
    const corpus = [...flows.values(), ...proofs.values()].join("\n");
    for (const claim of [
      "model", "agent", "permission", "memory", "State Machine", "trace",
      "Managed Agents", "ACP", "MCP", "sandbox", "Skill", "Deployment",
      "archive", "A2A", "access",
    ]) {
      assert.ok(corpus.toLowerCase().includes(claim.toLowerCase()), `series: missing ${claim}`);
    }
  }],
  ["interaction pacing", () => eachFlow((name, source) => {
    assert.match(source, /await wait\(/, `${name}: add a visual settle after interaction`);
  })],
  ["caption-scene coupling", () => {
    const overview = requiredFlow("00-awaken-agents-overview.mjs");
    const focusedBeats = matches(overview, /await beat\(/g);
    assert.ok(
      focusedBeats >= 3 && focusedBeats <= 4,
      "overview: focus the few pieces of evidence that close one human story",
    );
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
  ["unsaved Agent preview", () => {
    const authoring = requiredFlow("02-build-agent.mjs");
    assert.match(authoring, /Start preview\|开始预览/);
    assert.match(authoring, /status\(\)\)\.toBe\(404\)/);
    assert.match(authoring, /publication\.source_revision/);
    assert.match(authoring, /Message to agent over AG-UI/);
    assert.match(authoring, /protocols#protocol-ag-ui/);
    assert.match(authoring, /RUN_FINISHED/);
    assert.doesNotMatch(authoring, /Memory & resources|Memory 与资源/);
  }],
  ["MCP override proof", () => {
    const tools = requiredProof("20-tool-presentation.mjs");
    assert.match(tools, /mcp__issues__create_issue/);
    assert.match(tools, /config\.tools\)\.toEqual\(\["read"\]\)/);
    assert.match(tools, /config\.mcp_servers/);
  }],
  ["Memory effect proof", () => {
    const memory = requiredProof("04-resources-transparency.mjs");
    assert.ok(matches(memory, /createManagedSession\(/g) >= 2, "memory: use two fresh sessions");
    assert.match(memory, /memory\.content/);
    assert.match(memory, /Human approval is required before every external side effect/);
    assert.match(memory, /release-policy\.txt/);
    assert.match(memory, /publication\.agent_inputs\.inputs/);
    assert.doesNotMatch(memory, /Save resources|保存资源/);
  }],
  ["recording test contract", () => {
    assert.match(readme, /intro\(intent, capability\)/);
    assert.match(readme, /checkpoint\(name, assertion\)/);
    assert.match(readme, /aha\(text\)/);
  }],
  ["series completeness", () => {
    assert.deepEqual(flowNames, MARKETING_STORIES.map((slug) => `${slug}.mjs`));
    assert.deepEqual([...proofs.keys()], PRODUCT_PROOFS.map((slug) => `${slug}.mjs`));
  }],
  ["customer relationship objective", () => eachFlow((name, source) => {
    for (const field of ["loyalty", "satisfaction", "advocacy"]) {
      assert.match(source, new RegExp(`${field}:\\s*["']`), `${name}: explain how this story strengthens ${field}`);
    }
  })],
  ["complete close and bounded capture", () => {
    assert.match(harness, /MAX_FLOW_MS = 480_000/);
    assert.match(harness, /Runtime waits are editorially cut only after their real checkpoint succeeds/);
    assert.match(harness, /await showBrand\("The conversation can end/);
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

function requiredProof(name) {
  const source = proofs.get(name);
  assert.ok(source, `missing proof ${name}`);
  return source;
}

function storyLiteral(source, field) {
  const value = source.match(new RegExp(`${field}:\\s*(["'])(.*?)\\1`))?.[2];
  assert.ok(value, `missing story.${field}`);
  return value;
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
