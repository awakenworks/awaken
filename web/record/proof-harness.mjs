// Runs protocol and integration claims as real browser/API E2E tests without
// creating a marketing video. Usage: node proof-harness.mjs <proof-slug>.

import { chromium, expect } from "@playwright/test";
import { existsSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const slug = process.argv[2];
if (!slug) throw new Error("usage: node proof-harness.mjs <proof-slug>");
const BACKEND = process.env.BACKEND_URL ?? "http://127.0.0.1:38080";
const browserState = resolve(here, "../../.recording-awaken/browser-state.json");
const deadlineMs = 180_000;

const ready = await fetch(`${BACKEND}/readyz`, { signal: AbortSignal.timeout(4_000) });
if (!ready.ok) throw new Error(`all-in-one readyz returned HTTP ${ready.status}`);

const browser = await chromium.launch();
const contextOptions = {
  viewport: { width: 1440, height: 1000 },
};
const setupToken = process.env.AWAKEN_RECORD_SETUP_TOKEN?.trim();
let context;
if (setupToken) {
  const handoff = await browser.newContext(contextOptions);
  const exchange = await handoff.request.post(`${BACKEND}/v1/auth/local/exchange`, {
    data: { setup_token: setupToken },
  });
  if (exchange.ok()) {
    await handoff.storageState({ path: browserState });
    context = handoff;
  } else if (exchange.status() === 401) {
    // The setup token is intentionally one-time. The first process consumes it and persists the HttpOnly session for later cases.
    await handoff.close();
  } else {
    const detail = await exchange.text();
    await handoff.close();
    throw new Error(`proof browser setup exchange failed: HTTP ${exchange.status()} ${detail}`);
  }
}
context ??= await browser.newContext({
  ...contextOptions,
  ...(existsSync(browserState) ? { storageState: browserState } : {}),
});
context.setDefaultTimeout(15_000);
let page = await context.newPage();

const timeout = setTimeout(() => {
  console.error(`[proof] ${slug} exceeded ${deadlineMs}ms`);
  process.exit(124);
}, deadlineMs);
timeout.unref();

const api = {
  page,
  expect,
  checkpoint: async (name, assertion) => {
    await assertion();
    console.log(`[proof] ✓ ${name}`);
  },
  runtimeCheckpoint: async (name, assertion) => {
    await assertion();
    console.log(`[proof] ✓ ${name}`);
  },
  click: (locator) => locator.click(),
  cursorTo: (locator) => locator.hover(),
  // Proof runs do not render a cursor overlay. Keep the recording-side gesture
  // contract callable while the following semantic check/click owns the state change.
  tap: async () => {},
  type: async (locator, text) => {
    await locator.fill(text);
  },
  wait: (ms) => page.waitForTimeout(ms),
  say: async () => {},
  clearCaption: async () => {},
  intro: async () => {},
  aha: async () => {},
  beat: async (_text, target) => {
    if (target) await target.waitFor({ state: "visible" });
  },
  goto: async (path) => {
    await page.goto(`${BACKEND}${path}`, { waitUntil: "domcontentloaded" });
    await page.locator("main").waitFor({ state: "visible", timeout: 10_000 });
  },
};

try {
  const authenticated = await context.request.get(`${BACKEND}/v1/config/catalog`);
  if (!authenticated.ok()) {
    throw new Error(`proof browser is not authenticated: HTTP ${authenticated.status()}`);
  }
  const mod = await import(pathToFileURL(resolve(here, "proofs", `${slug}.mjs`)).href);
  if (typeof mod.prepare === "function") {
    await mod.prepare({ page: { request: context.request }, BACKEND });
  }
  await mod.run(api);
  console.log(`[proof] PASS ${slug}`);
} finally {
  clearTimeout(timeout);
  await context.close().catch(() => {});
  await browser.close().catch(() => {});
}
