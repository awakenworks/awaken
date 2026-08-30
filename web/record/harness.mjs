// Screen-recording harness for the Awaken console. Drives the REAL console and API
// from the same `awaken all-in-one` listener (:38080), records the browser to a .webm,
// and muxes it to a YouTube-ready .mp4 via ffmpeg. Runtime-effect chapters use
// either a selected live Provider Connection or an explicitly named deterministic
// control fixture; the copy and checkpoints must identify which proof is shown.
//
// It layers two demo affordances on top of the real DOM, both injected (never part
// of the product): an on-screen caption bar (the "narration", mirroring the example
// videos' captions.json) and a soft fake cursor that glides to each target so a
// viewer can follow the clicks. Usage: node harness.mjs <flow-slug>

import { chromium, expect } from "@playwright/test";
import { createHash } from "node:crypto";
import { mkdirSync, existsSync, mkdtempSync, readFileSync, renameSync, rmSync, writeFileSync } from "node:fs";
import { spawn } from "node:child_process";
import { fileURLToPath, pathToFileURL } from "node:url";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { performance } from "node:perf_hooks";

const here = dirname(fileURLToPath(import.meta.url));
const slug = process.argv[2];
if (!slug) {
  console.error("usage: harness.mjs <flow-slug>  (e.g. 01-connect-model)");
  process.exit(1);
}
const OWNS_RESTARTABLE_ALL_IN_ONE = slug === "05-survive-restart";
const RESTART_PORT = Number(process.env.AWAKEN_RECORD_RESTART_PORT ?? 38083);
const OWNED_ORIGIN = `http://127.0.0.1:${RESTART_PORT}`;
const BACKEND = OWNS_RESTARTABLE_ALL_IN_ONE ? OWNED_ORIGIN : process.env.BACKEND_URL ?? "http://127.0.0.1:38080";
const CONSOLE = OWNS_RESTARTABLE_ALL_IN_ONE ? OWNED_ORIGIN : process.env.CONSOLE_URL ?? BACKEND;
// Flow support modules resolve their API authority from BACKEND_URL when the
// dynamically imported story is evaluated. Bind the owned restart story to
// the same listener as its browser before loading any of those modules.
if (OWNS_RESTARTABLE_ALL_IN_ONE) {
  process.env.BACKEND_URL = BACKEND;
  process.env.CONSOLE_URL = CONSOLE;
}
// Match the existing Remotion demo baseline instead of quietly downgrading the
// release artifact to the smaller browser-recording default.
const SIZE = { width: 1920, height: 1200 };
const outDir = resolve(here, "out");
const repoRoot = resolve(here, "../..");
const browserState = resolve(here, "../../.recording-awaken/browser-state.json");
mkdirSync(outDir, { recursive: true });
const rawDir = resolve(outDir, `.raw-${slug}`);
rmSync(rawDir, { recursive: true, force: true });
for (const artifact of [slug, `${slug}.failed`]) {
  for (const extension of ["mp4", "webm", "captions.json", "vtt", "srt", "story.json", "png"]) {
    rmSync(resolve(outDir, `${artifact}.${extension}`), { force: true });
  }
  rmSync(resolve(outDir, `${artifact}.mp4.tmp.mp4`), { force: true });
}
const captions = [];
// Chromium's screencast clock and the subtitle wall clock do not preserve a
// laptop sleep in the same way. Keep macOS awake for the whole harness process
// instead of trying to reconstruct missing frames after the fact.
if (process.platform === "darwin" && process.env.AWAKEN_RECORD_ALLOW_SLEEP !== "1") {
  const sleepGuard = spawn("/usr/bin/caffeinate", ["-dims", "-w", String(process.pid)], {
    stdio: "ignore",
  });
  sleepGuard.once("error", (error) => {
    console.warn(`[record] could not acquire the macOS sleep guard: ${error.message}`);
  });
  sleepGuard.unref();
}
// Runtime waits are editorially cut only after their real checkpoint succeeds.
// Narrative length is governed by clarity and completeness, while eight minutes
// remains a hard active-time ceiling for collecting any one story.
const MAX_FLOW_MS = 480_000;
const MAX_PREPARE_MS = 480_000;
// Playwright's macOS context-close Promise can remain pending after Chromium has
// already written usable video bytes. Give graceful close 30 seconds, then let
// browser close finalize the file; the video probe remains publication authority.
const MAX_CONTEXT_CLOSE_MS = 30_000;
const MAX_CLOSE_MS = 300_000;
const MAX_ENCODE_MS = 600_000;
const MAX_PROBE_MS = 15_000;
const MAX_PROCESS_MS = 1_200_000;
let activeStory;
let flowSourceSha256;
const harnessSha256 = sha256(readFileSync(fileURLToPath(import.meta.url)));
let failureReason;
let videoStartedAt = 0;
let videoStartedActiveAt = 0;
let suspendedWallMs = 0;
let suspensionFailure;
let editorialCutMs = 0;
const videoCuts = [];

// A laptop sleep or suspended desktop task must not turn a healthy 45-second
// story into an immediate timeout on resume. Count responsive event-loop time;
// the parent process retains a separate wall-clock deadlock ceiling.
function activeDeadline(timeoutMs, onExpire, { unref = false } = {}) {
  let activeMs = 0;
  let previous = performance.now();
  const timer = setInterval(() => {
    const current = performance.now();
    const gap = current - previous;
    previous = current;
    activeMs += gap <= 5_000 ? gap : 1_000;
    if (activeMs < timeoutMs) return;
    clearInterval(timer);
    onExpire();
  }, 1_000);
  if (unref) timer.unref();
  return () => clearInterval(timer);
}

function execFileActive(command, args, timeoutMs) {
  return new Promise((resolvePromise, rejectPromise) => {
    const child = spawn(command, args, { stdio: ["ignore", "pipe", "pipe"] });
    let stdout = "";
    let stderr = "";
    let settled = false;
    const appendBounded = (current, chunk) => `${current}${chunk}`.slice(-1_000_000);
    child.stdout.on("data", (chunk) => { stdout = appendBounded(stdout, chunk); });
    child.stderr.on("data", (chunk) => { stderr = appendBounded(stderr, chunk); });
    const cancelDeadline = activeDeadline(timeoutMs, () => {
      if (settled) return;
      settled = true;
      child.kill("SIGKILL");
      const error = new Error(`${command} exceeded ${timeoutMs}ms of active execution`);
      error.stderr = stderr;
      rejectPromise(error);
    });
    child.once("error", (error) => {
      if (settled) return;
      settled = true;
      cancelDeadline();
      rejectPromise(error);
    });
    child.once("close", (code, signal) => {
      if (settled) return;
      settled = true;
      cancelDeadline();
      if (code === 0) return resolvePromise({ stdout, stderr });
      const error = new Error(`${command} exited with code ${code ?? "null"}${signal ? ` (${signal})` : ""}`);
      error.stderr = stderr;
      rejectPromise(error);
    });
  });
}

async function productRevision() {
  const revision = (await execFileActive(
    "git", ["-C", repoRoot, "rev-parse", "--verify", "HEAD"], 5_000,
  )).stdout.trim();
  if (!/^[0-9a-f]{40}$/.test(revision)) throw new Error("recording requires one exact Git revision");
  const trackedChanges = (await execFileActive(
    "git", ["-C", repoRoot, "status", "--porcelain", "--untracked-files=no"], 5_000,
  )).stdout.trim();
  if (trackedChanges) {
    throw new Error("recording requires a clean tracked source tree so the product revision is reproducible");
  }
  return revision;
}

const suspensionMonitor = (() => {
  let previousWall = Date.now();
  const timer = setInterval(() => {
    const currentWall = Date.now();
    const wallGap = currentWall - previousWall;
    // On macOS performance.now() advances across laptop sleep, so comparing
    // monotonic and wall clocks misses the exact failure this guard owns. The
    // one-second monitor itself is the authority: a gap above five seconds
    // means the recorder did not observe a continuous presentation.
    if (wallGap > 5_000) {
      const unobserved = wallGap - 1_000;
      suspendedWallMs += unobserved;
      suspensionFailure ??=
        `recording invalidated by system suspension or an unresponsive event loop (${Math.round(unobserved)}ms gap)`;
      console.error(`[record] ${suspensionFailure}`);
    }
    previousWall = currentWall;
  }, 1_000);
  timer.unref();
  return timer;
})();

function activeNow() {
  return Date.now() - suspendedWallMs;
}

function timelineNow() {
  return activeNow() - editorialCutMs;
}

function mergedVideoCuts() {
  const ordered = videoCuts
    .filter((cut) => cut.end_ms > cut.start_ms)
    .sort((left, right) => left.start_ms - right.start_ms);
  const merged = [];
  for (const cut of ordered) {
    const previous = merged.at(-1);
    if (!previous || cut.start_ms > previous.end_ms) merged.push({ ...cut });
    else previous.end_ms = Math.max(previous.end_ms, cut.end_ms);
  }
  return merged;
}

activeDeadline(MAX_PROCESS_MS, () => {
  console.error(`[record] timeout: process exceeded ${MAX_PROCESS_MS}ms; terminating with exit 124`);
  process.exit(124);
}, { unref: true });

async function stage(name, timeoutMs, action, onTimeout) {
  const started = activeNow();
  console.log(`[record] → ${name} (timeout ${timeoutMs}ms)`);
  let cancelDeadline;
  try {
    const result = await Promise.race([
      Promise.resolve().then(action),
      new Promise((_, reject) => {
        cancelDeadline = activeDeadline(timeoutMs, () => {
          Promise.resolve(onTimeout?.()).catch(() => {});
          reject(new Error(`${name} exceeded ${timeoutMs}ms`));
        });
      }),
    ]);
    console.log(`[record] ← ${name} (${activeNow() - started}ms)`);
    return result;
  } finally {
    cancelDeadline?.();
  }
}

let ownedAllInOne;
let recordedProductRevision;
try {
  recordedProductRevision = await productRevision();
  if (OWNS_RESTARTABLE_ALL_IN_ONE) ownedAllInOne = await createOwnedAllInOne();
  await preflightAllInOne();
} catch (error) {
  await ownedAllInOne?.stop().catch(() => {});
  console.error(`[record] all-in-one preflight failed at ${BACKEND}: ${error.message}`);
  process.exit(1);
}

const browser = await chromium.launch({ args: ["--force-color-profile=srgb"] });
const contextOptions = {
  viewport: SIZE,
  deviceScaleFactor: 1,
  colorScheme: "dark",
  recordVideo: { dir: rawDir, size: SIZE },
};
const setupToken = process.env.AWAKEN_RECORD_SETUP_TOKEN?.trim();
let ctx;
if (setupToken) {
  const handoff = await browser.newContext(contextOptions);
  const exchange = await handoff.request.post(`${BACKEND}/v1/auth/local/exchange`, {
    data: { setup_token: setupToken },
  });
  if (exchange.ok()) {
    await handoff.storageState({ path: browserState });
    ctx = handoff;
  } else if (exchange.status() === 401) {
    await handoff.close();
  } else {
    throw new Error(`local browser setup exchange failed: HTTP ${exchange.status()} ${await exchange.text()}`);
  }
}
ctx ??= await browser.newContext({
  ...contextOptions,
  ...(existsSync(browserState) ? { storageState: browserState } : {}),
});
ctx.setDefaultTimeout(15_000);
ctx.setDefaultNavigationTimeout(20_000);
// Default scope — go straight to the local Workspace after browser authentication.
await ctx.addInitScript(() => {
  try {
    localStorage.setItem("awaken.console.workspace", "");
    localStorage.setItem("awaken.console.locale", "en");
    localStorage.setItem("awaken.console.theme", "dark");
  } catch {
    /* first navigation may run before storage is available */
  }
});
let page;
let introCount = 0;
let checkpointCount = 0;
let ahaCount = 0;

// Current local Awaken uses a one-time setup handoff and an HttpOnly browser
// session. Exchange it into this context instead of injecting the long-lived
// management service credential into page JavaScript.
async function authenticateBrowser() {
  const catalog = await ctx.request.get(`${BACKEND}/v1/config/catalog`);
  if (!catalog.ok()) {
    if (!setupToken && catalog.status() === 401) {
      throw new Error(
        "backend requires local browser setup; set AWAKEN_RECORD_SETUP_TOKEN to the one-time token printed by `awaken all-in-one`",
      );
    }
    throw new Error(`authenticated catalog preflight failed: HTTP ${catalog.status()} ${await catalog.text()}`);
  }
}

// ---- injected demo chrome: caption bar + fake cursor (not product DOM) ----
async function installChrome() {
  await page.addStyleTag({
    content: `
      #rec-cap{position:fixed;left:50%;bottom:36px;transform:translateX(-50%);z-index:2147483646;
        max-width:78%;padding:14px 22px;border-radius:12px;font:500 20px/1.4 ui-sans-serif,system-ui,-apple-system,"Segoe UI",Roboto;
        color:#f1f6f8;background:rgba(14,23,28,.9);border:1px solid rgba(87,195,209,.38);
        box-shadow:0 8px 40px rgba(0,0,0,.5);backdrop-filter:blur(8px);text-align:center;
        opacity:0;transition:opacity .35s ease;pointer-events:none;letter-spacing:.2px}
      #rec-cap.on{opacity:1}
      #rec-cap.top{top:36px;bottom:auto}
      #rec-proof{position:fixed;right:28px;top:72px;z-index:2147483646;max-width:520px;
        padding:9px 13px;border-radius:9px;font:600 14px/1.35 ui-sans-serif,system-ui;
        color:#d6dde1;background:rgba(19,30,35,.94);border:1px solid rgba(87,195,209,.38);
        box-shadow:0 8px 30px rgba(0,0,0,.35);opacity:0;transform:translateY(-6px);
        transition:opacity .25s ease,transform .25s ease;pointer-events:none}
      #rec-proof.on{opacity:1;transform:translateY(0)}
      #rec-proof.low{top:auto;bottom:72px;transform:translateY(6px)}
      #rec-proof.low.on{transform:translateY(0)}
      #rec-proof.ok{color:#b8f7d0;border-color:rgba(80,220,140,.45)}
      #rec-proof.busy::before{content:"";display:inline-block;width:7px;height:7px;margin-right:8px;
        border-radius:50%;background:#57c3d1;animation:rec-pulse 1s ease-in-out infinite}
      #rec-cur{position:fixed;z-index:2147483647;width:22px;height:22px;left:0;top:0;margin:-11px 0 0 -11px;
        border-radius:50%;background:radial-gradient(circle at 35% 35%,#fff,#68ced9 60%,#2e8f9d);
        box-shadow:0 0 0 4px rgba(87,195,209,.25),0 3px 12px rgba(0,0,0,.5);
        transition:left .5s cubic-bezier(.4,0,.2,1),top .5s cubic-bezier(.4,0,.2,1);pointer-events:none}
      #rec-cur.tap{animation:rec-tap .4s ease}
      .rec-focus{outline:2px solid rgba(87,195,209,.82)!important;
        outline-offset:4px;animation:rec-focus 1.4s ease-in-out infinite alternate}
      #rec-brand{position:fixed;inset:0;z-index:2147483640;display:flex;align-items:center;justify-content:center;
        flex-direction:column;gap:20px;background:
          radial-gradient(circle at 50% 42%,rgba(63,179,194,.18),transparent 38%),
          radial-gradient(circle at 18% 18%,rgba(163,146,255,.1),transparent 30%),#0b1216;
        opacity:0;transition:opacity .45s ease;pointer-events:none}
      #rec-brand.on{opacity:1}
      #rec-brand-lockup{display:flex;align-items:center;gap:20px;color:#f1f6f8;text-shadow:0 12px 50px rgba(0,0,0,.45)}
      #rec-brand-lockup svg{width:92px;height:92px;filter:drop-shadow(0 8px 30px rgba(63,179,194,.16))}
      #rec-brand-copy{font:800 76px/.84 ui-sans-serif,system-ui,-apple-system,"Segoe UI";letter-spacing:-3px}
      #rec-brand-copy small{display:block;margin-top:12px;color:#93a0a7;font:650 19px/1 ui-sans-serif,system-ui;
        letter-spacing:8px;text-transform:uppercase}
      #rec-brand-line{max-width:980px;color:#d6dde1;font:500 30px/1.45 ui-sans-serif,system-ui;
        text-align:center;letter-spacing:.1px}
      @keyframes rec-tap{0%{transform:scale(1)}40%{transform:scale(.7)}100%{transform:scale(1)}}
      @keyframes rec-pulse{0%,100%{opacity:.35;transform:scale(.8)}50%{opacity:1;transform:scale(1.2)}}
      @keyframes rec-focus{from{box-shadow:0 0 0 0 rgba(87,195,209,.08)}to{box-shadow:0 0 0 9px rgba(87,195,209,.16)}}
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
      k.style.left = "960px";
      k.style.top = "600px";
      document.body.appendChild(k);
    }
    if (!document.getElementById("rec-proof")) {
      const p = document.createElement("div");
      p.id = "rec-proof";
      document.body.appendChild(p);
    }
    if (!document.getElementById("rec-brand")) {
      const brand = document.createElement("div");
      brand.id = "rec-brand";
      brand.innerHTML = '<div id="rec-brand-lockup"><svg viewBox="0 0 32 32" fill="none" aria-hidden="true"><path data-silhouette="A" d="M14.2 6h3.6l8.6 20h-3L16 8.4 8.6 26h-3Z" fill="#68ced9"></path><circle data-role="decision" cx="16" cy="20.2" r="2.5" fill="#a392ff"></circle></svg><div id="rec-brand-copy">Awaken<small>Agents</small></div></div><div id="rec-brand-line"></div>';
      document.body.appendChild(brand);
    }
  });
}
// Re-install after every full navigation (the injected nodes are wiped on reload).
function attachPageNavigationHook() {
  page.on("framenavigated", async (frame) => {
    if (frame === page.mainFrame()) await installChrome().catch(() => {});
  });
}

const wait = (ms) => page.waitForTimeout(ms);

function captionDuration(text, holdMs) {
  const readingMs = Math.min(4800, 850 + [...text].length * 28);
  return Math.min(5200, Math.max(holdMs ?? 0, readingMs));
}

async function trackSubtitle(text, holdMs) {
  const duration = captionDuration(text, holdMs);
  const startMs = timelineNow() - videoStartedActiveAt;
  captions.push({ start_ms: startMs, end_ms: startMs + duration, text });
  await wait(duration);
}

/** Show a caption ("narration") and hold for reading time proportional to length. */
async function say(text, holdMs) {
  await installChrome().catch(() => {});
  await page.evaluate(() => document.getElementById("rec-cap")?.classList.remove("on"));
  await wait(140);
  await page.evaluate((t) => {
    const c = document.getElementById("rec-cap");
    if (c) {
      c.textContent = t;
      c.classList.add("on");
    }
  }, text);
  await trackSubtitle(text, holdMs);
  await page.evaluate(() => document.getElementById("rec-cap")?.classList.remove("on"));
}
async function clearCaption() {
  await page.evaluate(() => document.getElementById("rec-cap")?.classList.remove("on"));
  await wait(250);
}

async function showBrand(line) {
  await installChrome().catch(() => {});
  await page.evaluate((text) => {
    const brand = document.getElementById("rec-brand");
    const subtitle = document.getElementById("rec-brand-line");
    const cursor = document.getElementById("rec-cur");
    if (subtitle) subtitle.textContent = text;
    if (cursor) cursor.style.opacity = "0";
    brand?.classList.add("on");
  }, line);
  await wait(500);
}

async function hideBrand() {
  await page.evaluate(() => {
    document.getElementById("rec-brand")?.classList.remove("on");
    const cursor = document.getElementById("rec-cur");
    if (cursor) cursor.style.opacity = "1";
  });
  await wait(500);
}

/** Keep narration and evidence visually coupled; the pulse prevents a caption beat
 * from becoming a static hold while showing exactly which product surface proves it. */
async function focus(locator) {
  await locator.waitFor({ state: "visible", timeout: 10_000 });
  await locator.scrollIntoViewIfNeeded({ timeout: 10_000 });
  await page.evaluate(() => document.querySelectorAll(".rec-focus").forEach((node) => node.classList.remove("rec-focus")));
  await locator.evaluate((node) => node.classList.add("rec-focus"));
  const box = await locator.boundingBox();
  await page.evaluate((targetIsLow) => {
    document.getElementById("rec-cap")?.classList.toggle("top", targetIsLow);
    document.getElementById("rec-proof")?.classList.toggle("low", targetIsLow);
  }, Boolean(box && box.y + box.height / 2 > SIZE.height * 0.58));
  await cursorTo(locator);
}

async function beat(text, locator, holdMs) {
  await focus(locator);
  await say(text, holdMs);
}

/** Establish the story before operating the UI: why this matters, then what Awaken does. */
async function intro(intent, capability) {
  introCount += 1;
  await showBrand(intent);
  // The story is already the hero line on the brand canvas. Keep it in the
  // exported subtitle tracks without repeating it in a second on-screen box.
  await trackSubtitle(intent, 3200);
  await hideBrand();
  await say(capability, 3800);
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
  const startedActive = activeNow();
  const startedWall = Date.now();
  const result = await checkpoint(name, async () => {
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
  const endedActive = activeNow();
  const endedWall = Date.now();
  const activeDuration = endedActive - startedActive;
  if (activeDuration > 8_000) {
    const cutStart = startedWall - videoStartedAt + 4_000;
    const cutEnd = endedWall - videoStartedAt - 2_000;
    if (cutEnd > cutStart) {
      videoCuts.push({ start_ms: cutStart, end_ms: cutEnd });
      editorialCutMs += activeDuration - 6_000;
      console.log(`[record] editorial cut: removed ${Math.round(activeDuration - 6_000)}ms of runtime waiting`);
    }
  }
  return result;
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
  await say(text, holdMs);
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
  // Evidence focus must never become an input authority. Clear the previous
  // outline before targeting the next control so a large focused container
  // cannot alter stacking or hit-testing for its sibling tabs and buttons.
  await page.evaluate(() => document.querySelectorAll(".rec-focus").forEach((node) => node.classList.remove("rec-focus")));
  await cursorTo(locator);
  await tap();
  await locator.click();
  await wait(400);
}
/** Type short labels visibly; fill long evidence atomically so recording cannot
 * fail because a browser spends the interaction budget synthesizing keystrokes. */
async function type(locator, text, opts = {}) {
  await page.evaluate(() => document.querySelectorAll(".rec-focus").forEach((node) => node.classList.remove("rec-focus")));
  await cursorTo(locator);
  await tap();
  await locator.click();
  await locator.fill("");
  if (text.length <= (opts.sequentialLimit ?? 80)) {
    await locator.pressSequentially(text, { delay: opts.delay ?? 28 });
  } else {
    await locator.fill(text);
  }
  await wait(text.length > (opts.sequentialLimit ?? 80) ? 700 : 300);
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
  restartAllInOne: async () => {
    if (!ownedAllInOne) throw new Error("this story does not own a restartable all-in-one process");
    return ownedAllInOne.restart();
  },
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
let flowRun;
try {
  await stage("browser authentication", 30_000, authenticateBrowser);
  const flowPath = resolve(here, "flows", `${slug}.mjs`);
  flowSourceSha256 = sha256(readFileSync(flowPath));
  const mod = await import(pathToFileURL(flowPath).href);
  activeStory = validateStory(mod.story);
  // Playwright begins a page's video at newPage(), not at the first navigation.
  // Perform deterministic control-plane preparation against the authenticated
  // context first so setup latency and a blank canvas never enter the release.
  if (typeof mod.prepare === "function") {
    await stage("pre-record Provider and fixture preparation", MAX_PREPARE_MS, () =>
      mod.prepare({ page: { request: ctx.request }, ctx, BACKEND }));
  }
  page = await ctx.newPage();
  api.page = page;
  videoStartedAt = Date.now();
  videoStartedActiveAt = activeNow();
  attachPageNavigationHook();
  await installChrome();
  let flowTimer;
  try {
    flowRun = Promise.resolve(mod.run(api));
    await stage("recorded story", MAX_FLOW_MS, () => flowRun, () => page?.close());
  } finally {
    clearTimeout(flowTimer);
  }
  if (introCount === 0) throw new Error("story contract failed: flow has no intent/capability intro");
  if (checkpointCount === 0) throw new Error("test contract failed: flow has no passing checkpoint");
  if (ahaCount !== 1) throw new Error(`story contract failed: expected exactly one AHA, got ${ahaCount}`);
  await showBrand("The conversation can end. The work stays ready for whoever comes next.");
  await say("Awaken Agents · work that can continue.", 3000);
  await clearCaption();
  await hideBrand();
} catch (e) {
  failed = true;
  failureReason = e instanceof Error ? e.message : String(e);
  console.error(`[record] flow ${slug} failed:`, failureReason);
  await installChrome().catch(() => {});
  await showProof(`✕ Recording stopped · ${failureReason}`, "", 1800).catch(() => {});
  await page?.screenshot({ path: resolve(outDir, `${slug}.failed.png`), fullPage: false }).catch(() => {});
} finally {
  // A timer cannot cancel an arbitrary flow Promise. Attach its terminal error
  // before closing Playwright so late page-closed failures cannot escape as an
  // unhandled rejection and suppress the failed-run video diagnostics.
  flowRun?.catch(() => {});
  await page?.waitForTimeout(600).catch(() => {});
  const video = page?.video();
  await stage("browser context close", MAX_CONTEXT_CLOSE_MS, () => ctx.close()).catch((error) => {
    console.warn(`[record] graceful context close did not finish; browser close will finalize video: ${error.message}`);
  });
  if (suspensionFailure && !failed) {
    failed = true;
    failureReason = suspensionFailure;
  }
  await stage("browser close", MAX_CLOSE_MS, () => browser.close()).catch((error) => {
    failed = true;
    failureReason ??= error.message;
    console.error(`[record] ${error.message}`);
  });
  if (ownedAllInOne) {
    await stage("owned all-in-one shutdown", 15_000, () => ownedAllInOne.stop()).catch((error) => {
      failed = true;
      failureReason ??= error.message;
      console.error(`[record] ${error.message}`);
    });
  }
  if (video) {
    const src = await stage("recorded video path", MAX_CLOSE_MS, () => video.path());
    let artifact = failed ? `${slug}.failed` : slug;
    let webm = resolve(outDir, `${artifact}.webm`);
    if (existsSync(src)) renameSync(src, webm);
    rmSync(rawDir, { recursive: true, force: true });
    // Mux atomically so a partial MP4 can never masquerade as a successful run.
    try {
      const mp4 = resolve(outDir, `${artifact}.mp4`);
      const temporaryMp4 = resolve(outDir, `${artifact}.mp4.tmp.mp4`);
      const encodeStarted = activeNow();
      console.log(`[record] → video encode (timeout ${MAX_ENCODE_MS}ms)`);
      // CRF 14 raises the visual-quality floor above the prior CRF 16 output;
      // `superfast` keeps the CRF 14 visual-quality target, resolution, frame
      // rate, and High profile. It spends a little more disk to keep dense UI
      // recordings inside the encode timeout on machines where `veryfast` can
      // fall below real time.
      const encoder = ["-c:v", "libx264", "-crf", "14", "-preset", "superfast", "-profile:v", "high"];
      const cuts = mergedVideoCuts();
      const suspensionShift = cuts
        .map(({ start_ms, end_ms }) =>
          `-${(end_ms - start_ms) / 1000}/TB*gte(T\\,${end_ms / 1000})`)
        .join("");
      const suspensionFilter = cuts
        .map(({ start_ms, end_ms }) => {
          const end = end_ms / 1000;
          const start = start_ms / 1000;
          return `not(between(t\\,${start}\\,${end}))`;
        })
        .join("*");
      const videoFilter = `${suspensionFilter ? `select=${suspensionFilter},` : ""}setpts=PTS${suspensionShift},fps=30`;
      await execFileActive(
        "ffmpeg",
        ["-y", "-i", webm, "-vf", videoFilter, ...encoder,
         "-level", "4.2", "-g", "60",
         "-pix_fmt", "yuv420p", "-movflags", "+faststart", temporaryMp4],
        MAX_ENCODE_MS,
      );
      console.log(`[record] ← video encode (${activeNow() - encodeStarted}ms)`);
      const probeStarted = activeNow();
      console.log(`[record] → video quality probe (timeout ${MAX_PROBE_MS}ms)`);
      await verifyFinalVideo(temporaryMp4);
      console.log(`[record] ← video quality probe (${activeNow() - probeStarted}ms)`);
      renameSync(temporaryMp4, mp4);
      if (process.env.AWAKEN_RECORD_KEEP_WEBM !== "1") rmSync(webm, { force: true });
      console.log(`[record] ✓ ${artifact}.mp4`);
    } catch (e) {
      const detail = String(e?.stderr ?? e?.message ?? e).trim().split("\n").slice(-4).join(" | ");
      failureReason = `ffmpeg failed: ${detail}`;
      if (!failed) {
        const failedWebm = resolve(outDir, `${slug}.failed.webm`);
        if (existsSync(webm)) renameSync(webm, failedWebm);
        webm = failedWebm;
        artifact = `${slug}.failed`;
      }
      failed = true;
      rmSync(resolve(outDir, `${slug}.mp4.tmp.mp4`), { force: true });
      rmSync(resolve(outDir, `${slug}.failed.mp4.tmp.mp4`), { force: true });
      console.error(`[record] kept ${webm}; ${failureReason}`);
    }
    writeCaptionArtifacts(artifact, captions);
    writeFileSync(resolve(outDir, `${artifact}.story.json`), `${JSON.stringify({
      ...activeStory,
      flow_source_sha256: flowSourceSha256,
      harness_sha256: harnessSha256,
      product_revision: recordedProductRevision,
      recorded_at: new Date().toISOString(),
      duration_ms: timelineNow() - videoStartedActiveAt,
      passed: !failed,
      ...(failureReason ? { failure: failureReason } : {}),
    }, null, 2)}\n`);
  }
}

function validateStory(story) {
  const fields = ["job", "stakes", "handoff", "promise", "effect", "aha", "loyalty", "satisfaction", "advocacy"];
  if (!story || typeof story !== "object") throw new Error("flow must export one story contract");
  for (const field of fields) {
    if (typeof story[field] !== "string" || story[field].trim().length < 20) {
      throw new Error(`story.${field} must explain a concrete customer outcome`);
    }
  }
  return story;
}

function sha256(content) {
  return createHash("sha256").update(content).digest("hex");
}
process.exit(failed ? 1 : 0);

async function preflightAllInOne() {
  const consoleOrigin = new URL(CONSOLE).origin;
  const backendOrigin = new URL(BACKEND).origin;
  if (consoleOrigin !== backendOrigin) {
    throw new Error(`recording requires one all-in-one origin; console=${consoleOrigin}, api=${backendOrigin}`);
  }
  const readiness = await fetch(`${BACKEND}/readyz`, { signal: AbortSignal.timeout(4000) });
  if (!readiness.ok) throw new Error(`readyz returned HTTP ${readiness.status}`);
  const shell = await fetch(`${CONSOLE}/`, { signal: AbortSignal.timeout(4000) });
  const html = await shell.text();
  if (!shell.ok || !shell.headers.get("content-type")?.includes("text/html") || !html.includes('id="root"')) {
    throw new Error("the API listener did not serve the embedded production console");
  }
}

async function createOwnedAllInOne() {
  const repository = resolve(here, "../..");
  const binary = process.env.AWAKEN_RECORD_RELEASE_BINARY?.trim()
    ? resolve(process.env.AWAKEN_RECORD_RELEASE_BINARY)
    : resolve(repository, process.env.CARGO_TARGET_DIR?.trim() || "target", "release", "awaken");
  if (!existsSync(binary)) {
    throw new Error(`restart story requires the release binary at ${binary}; build it first or set AWAKEN_RECORD_RELEASE_BINARY`);
  }
  const root = mkdtempSync(join(tmpdir(), "awaken-restart-recording-"));
  const config = join(root, "all-in-one.toml");
  writeFileSync(config, [
    'role = "all-in-one"',
    `bind = "127.0.0.1:${RESTART_PORT}"`,
    `data_dir = ${JSON.stringify(join(root, "data"))}`,
    "no_browser = true",
    'identity_mode = "no-login"',
    // This story executes a protected write. Use the same OS-enforced
    // recording boundary as the other marketing stories so Worker placement
    // can truthfully satisfy their read-only and write-policy requirements.
    'sandbox_tier = "namespace"',
    "acp_clis = []",
    "",
  ].join("\n"));

  const serviceEnvironment = { ...process.env };
  for (const name of Object.keys(serviceEnvironment)) {
    if (/(?:^|_)(?:API_KEY|ACCESS_TOKEN)$/i.test(name)
      || /^(?:GOOGLE_APPLICATION_CREDENTIALS|AZURE_OPENAI_API_KEY)$/i.test(name)) {
      delete serviceEnvironment[name];
    }
  }
  let host;
  let generation = 0;
  let output = "";

  const exitCode = (processHandle) => processHandle.exitCode != null
    ? Promise.resolve(processHandle.exitCode)
    : new Promise((resolveExit) => processHandle.once("exit", (code) => resolveExit(code ?? 1)));

  const start = async () => {
    output = "";
    host = spawn(binary, ["all-in-one", "--config", config], {
      cwd: root,
      env: serviceEnvironment,
      stdio: ["ignore", "pipe", "pipe"],
    });
    const append = (chunk) => { output = `${output}${chunk}`.slice(-200_000); };
    host.stdout.on("data", append);
    host.stderr.on("data", append);
    const deadline = Date.now() + 45_000;
    while (Date.now() < deadline) {
      if (host.exitCode != null) throw new Error(`owned all-in-one exited before readiness:\n${output}`);
      try {
        const ready = await fetch(`${OWNED_ORIGIN}/readyz`, { signal: AbortSignal.timeout(700) });
        if (ready.ok) {
          generation += 1;
          return host.pid;
        }
      } catch {
        // This bounded loop owns normal startup latency.
      }
      await new Promise((resolveWait) => setTimeout(resolveWait, 200));
    }
    throw new Error(`owned all-in-one did not become ready within 45000ms:\n${output}`);
  };

  const stopHost = async () => {
    if (!host || host.exitCode != null) return;
    host.kill("SIGINT");
    const graceful = await Promise.race([
      exitCode(host).then(() => true),
      new Promise((resolveWait) => setTimeout(() => resolveWait(false), 8_000)),
    ]);
    if (!graceful && host.exitCode == null) {
      host.kill("SIGKILL");
      await Promise.race([
        exitCode(host),
        new Promise((_, reject) => setTimeout(() => reject(new Error("owned all-in-one did not stop within 12000ms")), 4_000)),
      ]);
    }
  };

  await start();
  let closed = false;
  return {
    get generation() { return generation; },
    async restart() {
      if (closed) throw new Error("owned all-in-one is already closed");
      const beforePid = host.pid;
      await stopHost();
      const afterPid = await start();
      if (beforePid === afterPid) throw new Error("restart did not produce a new process incarnation");
      return { before_pid: beforePid, after_pid: afterPid, generation };
    },
    async stop() {
      if (closed) return;
      closed = true;
      try {
        await stopHost();
      } finally {
        rmSync(root, { recursive: true, force: true });
      }
    },
  };
}

async function verifyFinalVideo(path) {
  const probe = await execFileActive(
    "ffprobe",
    ["-v", "error", "-select_streams", "v:0", "-show_entries",
     "stream=codec_name,profile,width,height,pix_fmt,avg_frame_rate", "-show_entries",
     "format=duration", "-of", "json", path],
    MAX_PROBE_MS,
  );
  const result = JSON.parse(probe.stdout);
  const stream = result.streams?.[0];
  const [numerator, denominator] = String(stream?.avg_frame_rate ?? "0/1").split("/").map(Number);
  const fps = denominator ? numerator / denominator : 0;
  const duration = Number(result.format?.duration ?? 0);
  const defects = [
    stream?.codec_name !== "h264" && `codec=${stream?.codec_name}`,
    stream?.profile !== "High" && `profile=${stream?.profile}`,
    stream?.width !== SIZE.width && `width=${stream?.width}`,
    stream?.height !== SIZE.height && `height=${stream?.height}`,
    stream?.pix_fmt !== "yuv420p" && `pix_fmt=${stream?.pix_fmt}`,
    Math.abs(fps - 30) > 0.01 && `fps=${fps}`,
    (!Number.isFinite(duration) || duration <= 0) && `duration=${duration}`,
    Math.max(0, ...captions.map((caption) => caption.end_ms)) > duration * 1000 &&
      `captions exceed video duration (${Math.max(0, ...captions.map((caption) => caption.end_ms))}ms > ${duration * 1000}ms)`,
  ].filter(Boolean);
  if (defects.length > 0) throw new Error(`video quality gate failed: ${defects.join(", ")}`);
}

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
