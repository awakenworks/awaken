// Resume the complete series without deleting already-passed release artifacts.
// Missing external authority is reported as blocked by release-manifest.mjs;
// it is never replaced with a synthetic marketing claim.
import { spawnSync } from "node:child_process";
import { existsSync, readFileSync, rmSync } from "node:fs";
import { createRequire } from "node:module";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { MARKETING_STORIES } from "./catalog.mjs";

const here = dirname(fileURLToPath(import.meta.url));
const repo = resolve(here, "../..");
const out = resolve(here, "out");
// The child owns active-time stage limits. This larger wall-clock ceiling only
// catches a dead child; it deliberately tolerates a laptop sleep between frames.
const MAX_CHAPTER_PROCESS_MS = 1_800_000;
const requested = process.argv.slice(2).filter((argument) => argument !== "--");
const unknown = requested.filter((slug) => !MARKETING_STORIES.includes(slug));
if (unknown.length > 0) throw new Error(`unknown marketing story: ${unknown.join(", ")}`);
const slugs = requested.length > 0 ? requested : MARKETING_STORIES;
const manifestPath = resolve(out, "release-manifest.json");
rmSync(manifestPath, { force: true });
spawnSync(process.execPath, [resolve(here, "release-manifest.mjs")], {
  cwd: repo,
  env: process.env,
  stdio: "ignore",
});
const currentArtifacts = existsSync(manifestPath)
  ? new Set(JSON.parse(readFileSync(manifestPath, "utf8"))
    .chapters.filter((chapter) => chapter.status === "passed")
    .map((chapter) => chapter.slug))
  : new Set();
if (slugs.includes("03-connect-anthropic-sdk")) {
  const requireFromE2e = createRequire(resolve(repo, "e2e/package.json"));
  try {
    requireFromE2e.resolve("@anthropic-ai/sdk");
  } catch {
    throw new Error(
      "03-connect-anthropic-sdk requires the frozen E2E dependencies; run `npm ci --prefix e2e` before recording",
    );
  }
}
if (slugs.includes("05-survive-restart")) {
  const releaseBinary = process.env.AWAKEN_RECORD_RELEASE_BINARY?.trim()
    ? resolve(process.env.AWAKEN_RECORD_RELEASE_BINARY)
    : resolve(repo, process.env.CARGO_TARGET_DIR?.trim() || "target", "release", "awaken");
  if (!existsSync(releaseBinary)) {
    throw new Error(
      `05-survive-restart requires the release binary at ${releaseBinary}; build it first or set AWAKEN_RECORD_RELEASE_BINARY`,
    );
  }
}
const live = new Set([
  "00-awaken-agents-overview", "02-build-agent", "06-scheduled-operation",
]);
const hasLiveProvider = Boolean(
  process.env.GEMINI_PROJECT
  || (process.env.AWAKEN_RECORD_LIVE_PROVIDER && process.env.AWAKEN_RECORD_LIVE_API_KEY)
  || process.env.DEEPSEEK_API_KEY
  || process.env.OPENAI_API_KEY
);

let failed = false;
for (const slug of slugs) {
  if (process.env.AWAKEN_RECORD_FORCE !== "1" && currentArtifacts.has(slug)) {
    console.log(`[record:all] keep current passed artifact ${slug}`);
    continue;
  }
  if (live.has(slug) && !hasLiveProvider) {
    console.log(`[record:all] blocked ${slug}: no operator-authorized live Provider credential`);
    continue;
  }
  const script = "harness.mjs";
  const args = [resolve(here, script), slug];
  const result = spawnSync(process.execPath, args, {
    cwd: repo,
    env: process.env,
    stdio: "inherit",
    timeout: MAX_CHAPTER_PROCESS_MS,
    killSignal: "SIGKILL",
  });
  if (result.error?.code === "ETIMEDOUT") {
    console.error(`[record:all] timeout ${slug}: exceeded ${MAX_CHAPTER_PROCESS_MS}ms`);
  }
  if (result.status !== 0) failed = true;
}

const manifest = spawnSync(process.execPath, [resolve(here, "release-manifest.mjs")], { cwd: repo, env: process.env, stdio: "inherit" });
if (manifest.status !== 0) failed = true;
process.exit(failed ? 1 : 0);
