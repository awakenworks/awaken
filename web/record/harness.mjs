// Screen-recording harness for the Awaken console. Drives the REAL console (:3002)
// against a REAL backend (:38080) with Playwright, records the browser to a .webm,
// and muxes it to a YouTube-ready .mp4 via ffmpeg. Nothing is faked: the model
// replies are live (Vertex Gemini via gcloud OAuth, wired through the console's
// own Models/Credentials UI).
//
// It layers two demo affordances on top of the real DOM, both injected (never part
// of the product): an on-screen caption bar (the "narration", mirroring the example
// videos' captions.json) and a soft fake cursor that glides to each target so a
// viewer can follow the clicks. Usage: node harness.mjs <flow-slug>

import { chromium, expect } from "@playwright/test";
import { mkdirSync, existsSync, renameSync, rmSync, writeFileSync } from "node:fs";
import { execFileSync } from "node:child_process";
import { fileURLToPath, pathToFileURL } from "node:url";
import { dirname, resolve } from "node:path";

const here = dirname(fileURLToPath(import.meta.url));
const slug = process.argv[2];
if (!slug) {
  console.error("usage: harness.mjs <flow-slug>  (e.g. 01-connect-model)");
  process.exit(1);
}
const CONSOLE = process.env.CONSOLE_URL ?? "http://127.0.0.1:3002";
const BACKEND = process.env.BACKEND_URL ?? "http://127.0.0.1:38080";
const SIZE = { width: 1600, height: 1000 };
const outDir = resolve(here, "out");
mkdirSync(outDir, { recursive: true });
const rawDir = resolve(outDir, `.raw-${slug}`);
rmSync(rawDir, { recursive: true, force: true });
const recordStartedAt = Date.now();
const captions = [];
const MAX_VIDEO_MS = 180_000;
const MAX_FLOW_MS = 172_000; // reserve time for the branded close and final mux-safe settle
let activeStory;

try {
  const response = await fetch(`${BACKEND}/v1/config/catalog`, { signal: AbortSignal.timeout(4000) });
  if (!response.ok) throw new Error(`HTTP ${response.status}`);
} catch (error) {
  console.error(`[record] backend preflight failed at ${BACKEND}: ${error.message}`);
  process.exit(1);
}

const browser = await chromium.launch({ args: ["--force-color-profile=srgb"] });
const ctx = await browser.newContext({
  viewport: SIZE,
  deviceScaleFactor: 1,
  colorScheme: "dark",
  recordVideo: { dir: rawDir, size: SIZE },
});
// Default scope, no auth (backend runs open) — go straight to the workspace.
await ctx.addInitScript(() => {
  try {
    localStorage.setItem("awaken.console.workspace", "");
  } catch {
    /* first navigation may run before storage is available */
  }
});
const page = await ctx.newPage();
let introCount = 0;
let checkpointCount = 0;
let ahaCount = 0;

// ---- injected demo chrome: caption bar + fake cursor (not product DOM) ----
async function installChrome() {
  await page.addStyleTag({
    content: `
      #rec-cap{position:fixed;left:50%;bottom:36px;transform:translateX(-50%);z-index:2147483646;
        max-width:78%;padding:14px 22px;border-radius:12px;font:500 20px/1.4 ui-sans-serif,system-ui,-apple-system,"Segoe UI",Roboto;
        color:#eaf0ff;background:rgba(16,18,27,.86);border:1px solid rgba(120,140,220,.35);
        box-shadow:0 8px 40px rgba(0,0,0,.5);backdrop-filter:blur(8px);text-align:center;
        opacity:0;transition:opacity .35s ease;pointer-events:none;letter-spacing:.2px}
      #rec-cap.on{opacity:1}
      #rec-proof{position:fixed;right:28px;top:72px;z-index:2147483646;max-width:520px;
        padding:9px 13px;border-radius:9px;font:600 14px/1.35 ui-sans-serif,system-ui;
        color:#dbe7ff;background:rgba(18,24,38,.92);border:1px solid rgba(122,162,255,.35);
        box-shadow:0 8px 30px rgba(0,0,0,.35);opacity:0;transform:translateY(-6px);
        transition:opacity .25s ease,transform .25s ease;pointer-events:none}
      #rec-proof.on{opacity:1;transform:translateY(0)}
      #rec-proof.ok{color:#b8f7d0;border-color:rgba(80,220,140,.45)}
      #rec-proof.busy::before{content:"";display:inline-block;width:7px;height:7px;margin-right:8px;
        border-radius:50%;background:#7aa2ff;animation:rec-pulse 1s ease-in-out infinite}
      #rec-cur{position:fixed;z-index:2147483647;width:22px;height:22px;left:0;top:0;margin:-11px 0 0 -11px;
        border-radius:50%;background:radial-gradient(circle at 35% 35%,#fff,#7aa2ff 60%,#3b5bdb);
        box-shadow:0 0 0 4px rgba(122,162,255,.25),0 3px 12px rgba(0,0,0,.5);
        transition:left .5s cubic-bezier(.4,0,.2,1),top .5s cubic-bezier(.4,0,.2,1);pointer-events:none}
      #rec-cur.tap{animation:rec-tap .4s ease}
      .rec-focus{position:relative;z-index:3;outline:2px solid rgba(122,162,255,.78)!important;
        outline-offset:4px;animation:rec-focus 1.4s ease-in-out infinite alternate}
      @keyframes rec-tap{0%{transform:scale(1)}40%{transform:scale(.7)}100%{transform:scale(1)}}
      @keyframes rec-pulse{0%,100%{opacity:.35;transform:scale(.8)}50%{opacity:1;transform:scale(1.2)}}
      @keyframes rec-focus{from{box-shadow:0 0 0 0 rgba(122,162,255,.08)}to{box-shadow:0 0 0 9px rgba(122,162,255,.16)}}
    `,
  });
  await page.evaluate(() => {
    if (!document.getElementById("rec-cap")) {
      const c = document.createElement("div");
      c.id = "rec-cap";
      document.body.appendChild(c);
    }
    if (!document.getElementById("rec-cur")) {
      const k = document.createElement("div");
      k.id = "rec-cur";
      k.style.left = "800px";
      k.style.top = "500px";
      document.body.appendChild(k);
    }
    if (!document.getElementById("rec-proof")) {
      const p = document.createElement("div");
      p.id = "rec-proof";
      document.body.appendChild(p);
    }
  });
}
// Re-install after every full navigation (the injected nodes are wiped on reload).
page.on("framenavigated", async (f) => {
  if (f === page.mainFrame()) await installChrome().catch(() => {});
});

const wait = (ms) => page.waitForTimeout(ms);

/** Show a caption ("narration") and hold for reading time proportional to length. */
async function say(text, holdMs) {
  await installChrome().catch(() => {});
  await page.evaluate((t) => {
    const c = document.getElementById("rec-cap");
    if (c) {
      c.textContent = t;
      c.classList.add("on");
    }
  }, text);
  const readingMs = Math.min(4800, 850 + [...text].length * 28);
  const duration = Math.min(5200, Math.max(holdMs ?? 0, readingMs));
  const startMs = Date.now() - recordStartedAt;
  captions.push({ start_ms: startMs, end_ms: startMs + duration, text });
  await wait(duration);
}
async function clearCaption() {
  await page.evaluate(() => document.getElementById("rec-cap")?.classList.remove("on"));
  await wait(250);
}

/** Keep narration and evidence visually coupled; the pulse prevents a caption beat
 * from becoming a static hold while showing exactly which product surface proves it. */
async function focus(locator) {
  await locator.scrollIntoViewIfNeeded({ timeout: 3000 });
  await page.evaluate(() => document.querySelectorAll(".rec-focus").forEach((node) => node.classList.remove("rec-focus")));
  await locator.evaluate((node) => node.classList.add("rec-focus"));
  await cursorTo(locator);
}

async function beat(text, locator, holdMs) {
  await focus(locator);
  await say(text, holdMs);
}

/** Establish the story before operating the UI: why this matters, then what Awaken does. */
async function intro(intent, capability) {
  introCount += 1;
  await say(`Intent · ${intent}`, 3200);
  await say(`Awaken · ${capability}`, 3800);
  await clearCaption();
}

/** A recording is also a test: the named product claim must be observable or the run fails. */
async function checkpoint(name, assertion) {
  await showProof(`Live verification · ${name}`, "busy");
  try {
    await assertion();
    checkpointCount += 1;
    await showProof(`✓ Verified · ${name}`, "ok", 1500);
    console.log(`[record] ✓ checkpoint: ${name}`);
  } catch (error) {
    await showProof(`✕ Failed · ${name}`, "", 1800);
    throw new Error(`checkpoint failed — ${name}: ${error instanceof Error ? error.message : String(error)}`);
  }
}

/** A runtime claim must fail as soon as the product reports a terminal run error. */
async function runtimeCheckpoint(name, assertion) {
  return checkpoint(name, async () => {
    const terminalFailure = page.locator(".banner.warn").filter({
      hasText: /Run failed|运行失败|retries_exhausted/,
    }).first();
    await Promise.race([
      assertion(),
      terminalFailure.waitFor({ state: "visible", timeout: 0 }).then(async () => {
        const detail = (await terminalFailure.innerText()).replace(/\s+/g, " ").trim();
        throw new Error(`runtime failed before the claimed effect: ${detail}`);
      }),
    ]);
  });
}

async function showProof(text, tone, holdMs = 0) {
  await installChrome().catch(() => {});
  await page.evaluate(({ text, tone }) => {
    const proof = document.getElementById("rec-proof");
    if (!proof) return;
    proof.textContent = text;
    proof.className = `on ${tone}`;
  }, { text, tone });
  if (holdMs > 0) {
    await wait(holdMs);
    await page.evaluate(() => document.getElementById("rec-proof")?.classList.remove("on"));
  }
}

/** The shareable payoff. Every flow must visibly land one concise product truth. */
async function aha(text, holdMs = 4200) {
  if (activeStory?.aha && text !== activeStory.aha) {
    throw new Error(`AHA does not match story contract: expected "${activeStory.aha}"`);
  }
  ahaCount += 1;
  await say(`AHA · ${text}`, holdMs);
}

/** Glide the fake cursor to an element's centre (best-effort, purely visual). */
async function cursorTo(locator) {
  try {
    await locator.scrollIntoViewIfNeeded({ timeout: 3000 });
    const box = await locator.boundingBox();
    if (box) {
      await page.evaluate(
        ([x, y]) => {
          const k = document.getElementById("rec-cur");
          if (k) {
            k.style.left = `${x}px`;
            k.style.top = `${y}px`;
          }
        },
        [box.x + box.width / 2, box.y + box.height / 2],
      );
      await wait(560);
    }
  } catch {
    /* cursor is best-effort */
  }
}
async function tap() {
  await page.evaluate(() => {
    const k = document.getElementById("rec-cur");
    if (k) {
      k.classList.remove("tap");
      void k.offsetWidth;
      k.classList.add("tap");
    }
  });
  await wait(180);
}

/** Move-then-click, so the recording shows the cursor land on the target. */
async function click(locator) {
  await cursorTo(locator);
  await tap();
  await locator.click();
  await wait(400);
}
/** Type slowly enough to read, char-by-char. */
async function type(locator, text, opts = {}) {
  await cursorTo(locator);
  await tap();
  await locator.click();
  await locator.fill("");
  await locator.pressSequentially(text, { delay: opts.delay ?? 28 });
  await wait(300);
}

const api = {
  page,
  ctx,
  wait,
  say,
  clearCaption,
  focus,
  beat,
  intro,
  checkpoint,
  runtimeCheckpoint,
  aha,
  expect,
  click,
  type,
  cursorTo,
  tap,
  CONSOLE,
  SIZE,
};

async function goto(path) {
  // Session pages keep an SSE connection open; `networkidle` would manufacture a
  // 30-second static pause even though the UI is ready. Couple progress to visible
  // product content instead of transport silence.
  await page.goto(`${CONSOLE}${path}`, { waitUntil: "domcontentloaded" });
  await installChrome();
  await page.locator("main").waitFor({ state: "visible", timeout: 10_000 });
  await wait(500);
}
api.goto = goto;

let failed = false;
try {
  const mod = await import(pathToFileURL(resolve(here, "flows", `${slug}.mjs`)).href);
  activeStory = validateStory(mod.story);
  await installChrome();
  let flowTimer;
  try {
    await Promise.race([
      mod.run(api),
      new Promise((_, reject) => {
        flowTimer = setTimeout(() => reject(new Error("story exceeded its 172s execution budget")), MAX_FLOW_MS);
      }),
    ]);
  } finally {
    clearTimeout(flowTimer);
  }
  if (introCount === 0) throw new Error("story contract failed: flow has no intent/capability intro");
  if (checkpointCount === 0) throw new Error("test contract failed: flow has no passing checkpoint");
  if (ahaCount !== 1) throw new Error(`story contract failed: expected exactly one AHA, got ${ahaCount}`);
  await say("awaken · configure, prove, and run agents — fully in the browser.", 3000);
  await clearCaption();
  const elapsed = Date.now() - recordStartedAt;
  if (elapsed > MAX_VIDEO_MS) throw new Error(`video is ${elapsed}ms; every story must close within 180000ms`);
} catch (e) {
  failed = true;
  console.error(`[record] flow ${slug} failed:`, e.message);
} finally {
  await wait(600);
  const video = page.video();
  await ctx.close();
  await browser.close();
  if (video) {
    const src = await video.path();
    const artifact = failed ? `${slug}.failed` : slug;
    writeCaptionArtifacts(artifact, captions);
    writeFileSync(resolve(outDir, `${artifact}.story.json`), `${JSON.stringify({
      ...activeStory,
      duration_ms: Date.now() - recordStartedAt,
      passed: !failed,
    }, null, 2)}\n`);
    const webm = resolve(outDir, `${artifact}.webm`);
    if (existsSync(src)) renameSync(src, webm);
    rmSync(rawDir, { recursive: true, force: true });
    // Mux to H.264 mp4 (YouTube-ready, faststart). Falls back to keeping the webm.
    try {
      const mp4 = resolve(outDir, `${artifact}.mp4`);
      execFileSync(
        "ffmpeg",
        ["-y", "-i", webm, "-c:v", "libx264", "-crf", "18", "-preset", "medium",
         "-pix_fmt", "yuv420p", "-movflags", "+faststart", mp4],
        { stdio: "ignore" },
      );
      console.log(`[record] ✓ ${artifact}.mp4`);
    } catch (e) {
      console.log(`[record] kept ${slug}.webm (ffmpeg failed: ${e.message})`);
    }
  }
}

function validateStory(story) {
  const fields = ["promise", "effect", "aha", "loyalty", "satisfaction", "advocacy"];
  if (!story || typeof story !== "object") throw new Error("flow must export one story contract");
  for (const field of fields) {
    if (typeof story[field] !== "string" || story[field].trim().length < 20) {
      throw new Error(`story.${field} must explain a concrete customer outcome`);
    }
  }
  return story;
}
process.exit(failed ? 1 : 0);

function writeCaptionArtifacts(artifact, entries) {
  const timed = entries.map((entry, index) => ({ ...entry, index: index + 1 }));
  writeFileSync(resolve(outDir, `${artifact}.captions.json`), `${JSON.stringify(timed, null, 2)}\n`);
  const vtt = ["WEBVTT", "", ...timed.flatMap((entry) => [
    `${formatTime(entry.start_ms, ".")} --> ${formatTime(entry.end_ms, ".")}`,
    entry.text,
    "",
  ])].join("\n");
  writeFileSync(resolve(outDir, `${artifact}.vtt`), vtt);
  const srt = timed.flatMap((entry) => [
    String(entry.index),
    `${formatTime(entry.start_ms, ",")} --> ${formatTime(entry.end_ms, ",")}`,
    entry.text,
    "",
  ]).join("\n");
  writeFileSync(resolve(outDir, `${artifact}.srt`), srt);
}

function formatTime(milliseconds, separator) {
  const ms = Math.max(0, Math.round(milliseconds));
  const hours = Math.floor(ms / 3_600_000);
  const minutes = Math.floor((ms % 3_600_000) / 60_000);
  const seconds = Math.floor((ms % 60_000) / 1000);
  const fraction = ms % 1000;
  return `${String(hours).padStart(2, "0")}:${String(minutes).padStart(2, "0")}:${String(seconds).padStart(2, "0")}${separator}${String(fraction).padStart(3, "0")}`;
}
