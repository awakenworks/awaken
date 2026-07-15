// Screen-recording harness for the Awaken console. Drives the REAL console (:3002)
// against a REAL backend (:38080) with Playwright, records the browser to a .webm,
// and muxes it to a YouTube-ready .mp4 via ffmpeg. Nothing is faked: the model
// replies are live (KIMI, wired through the console's own Models/Credentials UI).
//
// It layers two demo affordances on top of the real DOM, both injected (never part
// of the product): an on-screen caption bar (the "narration", mirroring the example
// videos' captions.json) and a soft fake cursor that glides to each target so a
// viewer can follow the clicks. Usage: node harness.mjs <flow-slug>

import { chromium } from "@playwright/test";
import { mkdirSync, existsSync, renameSync, rmSync } from "node:fs";
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
const SIZE = { width: 1600, height: 1000 };
const outDir = resolve(here, "out");
mkdirSync(outDir, { recursive: true });
const rawDir = resolve(outDir, `.raw-${slug}`);
rmSync(rawDir, { recursive: true, force: true });

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
      #rec-cur{position:fixed;z-index:2147483647;width:22px;height:22px;left:0;top:0;margin:-11px 0 0 -11px;
        border-radius:50%;background:radial-gradient(circle at 35% 35%,#fff,#7aa2ff 60%,#3b5bdb);
        box-shadow:0 0 0 4px rgba(122,162,255,.25),0 3px 12px rgba(0,0,0,.5);
        transition:left .5s cubic-bezier(.4,0,.2,1),top .5s cubic-bezier(.4,0,.2,1);pointer-events:none}
      #rec-cur.tap{animation:rec-tap .4s ease}
      @keyframes rec-tap{0%{transform:scale(1)}40%{transform:scale(.7)}100%{transform:scale(1)}}
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
  await wait(holdMs ?? Math.min(6000, 1400 + text.length * 45));
}
async function clearCaption() {
  await page.evaluate(() => document.getElementById("rec-cap")?.classList.remove("on"));
  await wait(250);
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

const api = { page, ctx, wait, say, clearCaption, click, type, cursorTo, tap, CONSOLE, SIZE };

async function goto(path) {
  await page.goto(`${CONSOLE}${path}`, { waitUntil: "networkidle" });
  await installChrome();
  await wait(500);
}
api.goto = goto;

let failed = false;
try {
  const mod = await import(pathToFileURL(resolve(here, "flows", `${slug}.mjs`)).href);
  await installChrome();
  await mod.run(api);
  await say("awaken · configure, prove, and run agents — fully in the browser.", 3800);
  await clearCaption();
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
    const webm = resolve(outDir, `${slug}.webm`);
    if (existsSync(src)) renameSync(src, webm);
    rmSync(rawDir, { recursive: true, force: true });
    // Mux to H.264 mp4 (YouTube-ready, faststart). Falls back to keeping the webm.
    try {
      const mp4 = resolve(outDir, `${slug}.mp4`);
      execFileSync(
        "ffmpeg",
        ["-y", "-i", webm, "-c:v", "libx264", "-crf", "18", "-preset", "medium",
         "-pix_fmt", "yuv420p", "-movflags", "+faststart", mp4],
        { stdio: "ignore" },
      );
      console.log(`[record] ✓ ${slug}.mp4`);
    } catch (e) {
      console.log(`[record] kept ${slug}.webm (ffmpeg failed: ${e.message})`);
    }
  }
}
process.exit(failed ? 1 : 0);
