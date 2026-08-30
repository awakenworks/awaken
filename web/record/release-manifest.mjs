import { execFileSync } from "node:child_process";
import { createHash } from "node:crypto";
import { existsSync, readFileSync, writeFileSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { MARKETING_STORIES, PRODUCT_PROOFS, PROOF_REENTRY_RULE } from "./catalog.mjs";

const here = dirname(fileURLToPath(import.meta.url));
const repo = resolve(here, "../..");
const outDir = resolve(here, "out");
const currentProductRevision = execFileSync("git", ["rev-parse", "HEAD"], {
  cwd: repo,
  encoding: "utf8",
}).trim();
const harnessSha256 = createHash("sha256").update(readFileSync(resolve(here, "harness.mjs"))).digest("hex");
const slugs = MARKETING_STORIES;
const live = new Set([
  "00-awaken-agents-overview", "02-build-agent", "06-scheduled-operation",
]);

const chapters = slugs.map((slug) => inspect(slug));
const productRevisions = [...new Set(chapters.flatMap((chapter) => chapter.product_revision ?? []))];
const manifest = {
  generated_at: new Date().toISOString(),
  quality_floor: { codec: "h264", profile: "High", width: 1920, height: 1200, pixel_format: "yuv420p", fps: 30 },
  passed: chapters.filter((chapter) => chapter.status === "passed").length,
  blocked: chapters.filter((chapter) => chapter.status !== "passed").length,
  all_publishable: chapters.every((chapter) => chapter.status === "passed")
    && productRevisions.length === 1
    && productRevisions[0] === currentProductRevision,
  current_product_revision: currentProductRevision,
  product_revisions: productRevisions,
  chapters,
  proof_only: {
    cases: PRODUCT_PROOFS,
    recorded: false,
    reentry_rule: PROOF_REENTRY_RULE,
  },
};
writeFileSync(resolve(outDir, "release-manifest.json"), `${JSON.stringify(manifest, null, 2)}\n`);
console.log(`[manifest] ${manifest.passed}/${chapters.length} publishable; ${manifest.blocked} blocked`);
process.exit(manifest.all_publishable ? 0 : 1);

function inspect(slug) {
  const mp4 = resolve(outDir, `${slug}.mp4`);
  const storyPath = resolve(outDir, `${slug}.story.json`);
  if (!existsSync(mp4) || !existsSync(storyPath)) {
    return { slug, status: "blocked", reason: blockedReason(slug) };
  }
  const story = JSON.parse(readFileSync(storyPath, "utf8"));
  if (story.passed !== true) return { slug, status: "blocked", reason: story.failure ?? "story contract did not pass" };
  if (!/^[0-9a-f]{40}$/.test(story.product_revision ?? "")) {
    return { slug, status: "blocked", reason: "finished video is not bound to an exact Awaken revision" };
  }
  if (story.product_revision !== currentProductRevision) {
    return {
      slug,
      status: "blocked",
      reason: `finished video targets ${story.product_revision}; current product revision is ${currentProductRevision}`,
    };
  }
  const flowSha256 = createHash("sha256").update(readFileSync(resolve(here, "flows", `${slug}.mjs`))).digest("hex");
  if (story.flow_source_sha256 !== flowSha256 || story.harness_sha256 !== harnessSha256) {
    return { slug, status: "blocked", reason: "finished video was recorded from an older story or harness" };
  }
  const probe = JSON.parse(execFileSync("ffprobe", [
    "-v", "error", "-select_streams", "v:0", "-show_entries",
    "stream=codec_name,profile,width,height,pix_fmt,avg_frame_rate", "-show_entries",
    "format=duration,size", "-of", "json", mp4,
  ], { encoding: "utf8" }));
  const stream = probe.streams?.[0] ?? {};
  const [n, d] = String(stream.avg_frame_rate ?? "0/1").split("/").map(Number);
  const fps = d ? n / d : 0;
  const quality = {
    codec: stream.codec_name,
    profile: stream.profile,
    width: stream.width,
    height: stream.height,
    pixel_format: stream.pix_fmt,
    fps,
    duration_seconds: Number(probe.format?.duration ?? 0),
    bytes: Number(probe.format?.size ?? 0),
  };
  const valid = quality.codec === "h264" && quality.profile === "High"
    && quality.width === 1920 && quality.height === 1200
    && quality.pixel_format === "yuv420p" && Math.abs(quality.fps - 30) < 0.01
    && quality.duration_seconds > 0;
  if (!valid) return { slug, status: "blocked", reason: "finished video is below the inherited quality floor", quality };
  return {
    slug,
    status: "passed",
    promise: story.promise,
    effect: story.effect,
    product_revision: story.product_revision,
    recorded_at: story.recorded_at,
    quality,
    sha256: createHash("sha256").update(readFileSync(mp4)).digest("hex"),
  };
}

function blockedReason(slug) {
  if (live.has(slug)) return "external live-model traffic requires explicit operator authorization for an available Provider credential";
  return "recording has not completed its real all-in-one checkpoints";
}
