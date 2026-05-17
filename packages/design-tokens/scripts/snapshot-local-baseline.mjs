#!/usr/bin/env node
/**
 * Snapshot LOCAL Awaken tokens into
 * `packages/design-tokens/baseline/teams-shared/`, renaming the `aw.`
 * namespace to `os.` so it has the shape teams' upstream tokens carry.
 *
 *      ┌─────────────────────────────────────────────────────────────┐
 *      │  THIS IS NOT A CROSS-REPO PULL.                              │
 *      │  It only freezes what Awaken currently considers correct.   │
 *      │  Run AFTER you have manually confirmed the shared token     │
 *      │  values match teams' upstream — otherwise drift propagates  │
 *      │  silently from Awaken into the baseline.                    │
 *      └─────────────────────────────────────────────────────────────┘
 *
 * Why we still vendor a baseline:
 *   `tokens.parity.test.ts` diffs `tokens/` against this baseline.
 *   It catches *internal* Awaken drift: if anyone changes a shared file
 *   without re-snapshotting (i.e. without confirming the change is
 *   acceptable for teams too), the test fails. That's a guardrail, not
 *   a substitute for fetching teams' actual JSON.
 *
 * Better future workflow (left as a follow-up):
 *   Accept a `--from-teams <path-or-tarball>` argument that reads from a
 *   real teams checkout and writes the upstream commit SHA into the
 *   baseline metadata. Until that lands, treat this script as a manual
 *   maintainer step that gets reviewed in the PR diff.
 *
 * Transform: top-level `aw` key → `os`, and DTCG references
 * `{aw.x.y}` → `{os.x.y}`, matching teams' namespace.
 */

import fs from "node:fs/promises";
import path from "node:path";
import { execSync } from "node:child_process";
import { fileURLToPath } from "node:url";

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const pkgRoot = path.resolve(__dirname, "..");
const srcRoot = path.join(pkgRoot, "tokens");
const baselineRoot = path.join(pkgRoot, "baseline/teams-shared");
const metaPath = path.join(pkgRoot, "baseline/SNAPSHOT.json");

const SHARED_FILES = [
  "primitives/motion.json",
  "primitives/phase.json",
  "primitives/radius.json",
  "primitives/spacing.json",
  "primitives/typography.json",
  "primitives/colors.json",
  "semantic/sizing.json",
  "semantic/phase-chrome.json",
];

function rewrite(node) {
  if (typeof node === "string") {
    return node.replace(/\{aw\./g, "{os.");
  }
  if (node === null || typeof node !== "object") return node;
  if (Array.isArray(node)) return node.map(rewrite);
  const out = {};
  for (const [k, v] of Object.entries(node)) {
    const renamed = k === "aw" ? "os" : k;
    out[renamed] = rewrite(v);
  }
  return out;
}

function gitInfo() {
  try {
    const sha = execSync("git rev-parse HEAD", { cwd: pkgRoot, encoding: "utf8" }).trim();
    const branch = execSync("git rev-parse --abbrev-ref HEAD", { cwd: pkgRoot, encoding: "utf8" }).trim();
    const dirty = execSync("git status --porcelain tokens", {
      cwd: pkgRoot,
      encoding: "utf8",
    }).trim().length > 0;
    return { sha, branch, dirty };
  } catch {
    return { sha: "unknown", branch: "unknown", dirty: false };
  }
}

async function main() {
  await fs.rm(baselineRoot, { recursive: true, force: true });
  for (const rel of SHARED_FILES) {
    const src = path.join(srcRoot, rel);
    const dst = path.join(baselineRoot, rel);
    const json = JSON.parse(await fs.readFile(src, "utf8"));
    const transformed = rewrite(json);
    await fs.mkdir(path.dirname(dst), { recursive: true });
    await fs.writeFile(
      dst,
      JSON.stringify(transformed, null, 2) + "\n",
      "utf8",
    );
    console.log(`✔︎ baseline/teams-shared/${rel}`);
  }

  const meta = {
    source: "local-awaken",
    purpose:
      "Snapshot of Awaken's own shared tokens, NOT pulled from teams upstream. " +
      "Parity test treats this as the agreed-upon baseline for drift detection within Awaken.",
    generated_at: new Date().toISOString(),
    awaken_git: gitInfo(),
    files: SHARED_FILES,
  };
  await fs.writeFile(metaPath, JSON.stringify(meta, null, 2) + "\n", "utf8");
  console.log(`✔︎ baseline/SNAPSHOT.json`);
  console.log("");
  console.log("Reminder: this script does NOT fetch teams upstream. Before");
  console.log("snapshotting, manually confirm the shared token values match");
  console.log("teams/web/design-tokens — otherwise drift goes undetected.");
}

main().catch((e) => {
  console.error(e);
  process.exit(1);
});
