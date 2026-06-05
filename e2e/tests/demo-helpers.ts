import type { Page } from '@playwright/test';
import fs from 'node:fs';
import path from 'node:path';
import { en } from '../../apps/admin-console/src/lib/i18n/en';
import { zhCN } from '../../apps/admin-console/src/lib/i18n/zh-CN';

/** Source-frame geometry: viewport 1600x1000 at deviceScaleFactor 2. */
export const DSF = 2;
export const SRC_WIDTH = 1600 * DSF;
export const SRC_HEIGHT = 1000 * DSF;

const LOCALE_DIR = (process.env.DEMO_LOCALE === 'zh' || process.env.DEMO_LOCALE === 'zh-CN')
  ? 'zh'
  : 'en';

/** cwd is the e2e/ package when Playwright runs. */
export const FRAMES_DIR = path.resolve(process.cwd(), 'target/demo-frames', LOCALE_DIR);

type Transition = 'fade' | 'slide' | 'cut';
interface ManifestShot {
  scene: string;
  index: number;
  image?: string;
  caption?: string;
  hold: number;
  cursor?: { x: number; y: number };
  click?: boolean;
  focus?: { x: number; y: number; w: number; h: number };
  transition?: Transition;
  title?: string;
  subtitle?: string;
  link?: string;
}

const recordedShots: ManifestShot[] = [];
const sceneCounters = new Map<string, number>();
let currentScene = 'init';
let pendingCaption: string | undefined;

/**
 * Promo-recording helpers for `admin-demo.spec.ts`.
 *
 * These exist purely to make Playwright's native `recordVideo` output look like
 * a hand-driven product demo: a visible cursor (Playwright never renders the OS
 * pointer), an on-screen scene caption, and deliberate pacing so the real-time
 * capture is watchable instead of a blur of instant DOM mutations.
 */

export type DemoLocale = 'en' | 'zh-CN';

export const DEMO_LOCALE: DemoLocale =
  process.env.DEMO_LOCALE === 'zh' || process.env.DEMO_LOCALE === 'zh-CN'
    ? 'zh-CN'
    : 'en';

export const ADMIN_TOKEN = process.env.DEMO_ADMIN_TOKEN ?? 'demo-bearer-token';

export const BACKEND_URL =
  process.env.AWAKEN_BACKEND_URL ?? 'http://127.0.0.1:38080';

/** Pick the locale-appropriate string from a bilingual pair. */
export function tr(pair: { en: string; zh: string }): string {
  return DEMO_LOCALE === 'zh-CN' ? pair.zh : pair.en;
}

/**
 * English→Chinese lookup built by walking the app's own i18n dictionaries
 * (identical shape). Lets the spec keep writing English accessible
 * names/labels while matching the localized UI. Strings that are hardcoded in
 * components (not in the dict) fall through unchanged — which is correct,
 * because those render in English in both locales.
 */
const EN_TO_ZH: Map<string, string> = (() => {
  const map = new Map<string, string>();
  const walk = (a: any, b: any) => {
    if (!a || !b) return;
    for (const k of Object.keys(a)) {
      const av = a[k];
      const bv = b[k];
      if (typeof av === 'string') {
        if (typeof bv === 'string') map.set(av, bv);
      } else if (av && typeof av === 'object') {
        walk(av, bv);
      }
    }
  };
  walk(en, zhCN);
  return map;
})();

/** Resolve an English UI string to the active locale's string. */
export function L(enStr: string): string {
  return DEMO_LOCALE === 'zh-CN' ? EN_TO_ZH.get(enStr) ?? enStr : enStr;
}

/** Resolve to a RegExp matching the active-locale string (escaped). */
export function Lre(enStr: string): RegExp {
  return new RegExp(L(enStr).replace(/[.*+?^${}()|[\]\\]/g, '\\$&'));
}

/**
 * RegExp matching EITHER the English string OR its translation. Robust against
 * the app's mixed reality where some controls are i18n'd and others are
 * hardcoded English (so a single value-based translation can't tell which).
 * `anchor` wraps in ^…$ for exact-style matching (use for field labels to avoid
 * substring collisions like "Command" vs "Open command palette").
 */
export function Lboth(enStr: string, anchor = false): RegExp {
  const esc = (s: string) => s.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
  const zh = L(enStr);
  const alts = zh === enStr ? [enStr] : [enStr, zh];
  const body = alts.map(esc).join('|');
  return new RegExp(anchor ? `^(?:${body})$` : body);
}

/** A short, watchable pause. Pacing is everything in a screen recording. */
export function beat(page: Page, ms = 650): Promise<void> {
  return page.waitForTimeout(ms);
}

/**
 * Seed admin token + locale + a comfortable demo theme into localStorage before
 * the app boots, and inject the visible cursor + caption layer on every
 * document. Must be called before the first `page.goto`.
 */
export async function primeDemoPage(page: Page): Promise<void> {
  await page.addInitScript(
    ({ token, locale }) => {
      localStorage.setItem('awaken.adminToken', token);
      localStorage.setItem('awaken.admin.locale', locale);
      localStorage.setItem('awaken.admin.theme', 'dark');
    },
    { token: ADMIN_TOKEN, locale: DEMO_LOCALE },
  );

  // Paint the root dark immediately so the pre-React blank body never flashes
  // white between navigations. Cursor + captions are now drawn by Remotion, so
  // nothing is injected into the page (screenshots stay clean).
  await page.addInitScript(() => {
    try {
      const root = document.documentElement;
      root.style.background = '#020617';
      (root.style as any).colorScheme = 'dark';
      const darkStyle = document.createElement('style');
      darkStyle.textContent = 'html,body{background:#020617}';
      root.appendChild(darkStyle);
    } catch {
      /* document not ready — best effort */
    }
  });
}

/** Record a beat with a (locale-resolved) caption over the current screen. */
export async function caption(page: Page, text: string): Promise<void> {
  await shot(page, { caption: text });
}

/** A scene-title beat: longer hold, fade in. */
export async function scene(
  page: Page,
  pair: { en: string; zh: string },
): Promise<void> {
  await shot(page, { caption: tr(pair), hold: 3.0, transition: 'fade' });
}

/** Record a beat with the cursor resting on the given element, highlighting it. */
export async function point(page: Page, selector: string): Promise<void> {
  const el = page.locator(selector).first();
  await el.scrollIntoViewIfNeeded().catch(() => {});
  const cursor = await targetOf(page, selector);
  const focus = await rectOf(page, selector);
  await shot(page, { cursor, focus });
}

/** Type into a field at a human, on-camera speed. */
export async function typeSlow(
  locator: ReturnType<Page['locator']>,
  text: string,
  delay = 38,
): Promise<void> {
  await locator.click();
  await locator.fill('');
  await locator.pressSequentially(text, { delay });
}

/** Smoothly scroll the window so long pages read as a pan, not a jump. */
export async function smoothScroll(page: Page, to: number, steps = 24): Promise<void> {
  await page.evaluate(
    async ({ to, steps }) => {
      const start = window.scrollY;
      const delta = (to - start) / steps;
      for (let i = 0; i < steps; i++) {
        window.scrollBy(0, delta);
        await new Promise((r) => setTimeout(r, 16));
      }
    },
    { to, steps },
  );
  await beat(page, 250);
}

/** Reset state and the frames dir at the start of a capture run. */
export function initCapture(): void {
  fs.rmSync(FRAMES_DIR, { recursive: true, force: true });
  fs.mkdirSync(FRAMES_DIR, { recursive: true });
  recordedShots.length = 0;
  sceneCounters.clear();
  currentScene = 'init';
  pendingCaption = undefined;
}

/** Tell the recorder which scene upcoming shots belong to. Resets the persisted
 *  caption so a new scene's first (nav) shot never inherits the prior scene's
 *  caption. */
export function setCurrentScene(name: string): void {
  currentScene = name;
  pendingCaption = undefined;
}

/** Centered 2x-pixel cursor target for a selector, or undefined if not found. */
export async function targetOf(
  page: Page,
  selector: string,
): Promise<{ x: number; y: number } | undefined> {
  const box = await page.locator(selector).first().boundingBox().catch(() => null);
  if (!box) return undefined;
  return { x: (box.x + box.width / 2) * DSF, y: (box.y + box.height / 2) * DSF };
}

/** Full 2x-pixel element rect for a selector — drives the camera framing and the
 *  highlight ring. Returns undefined if the element isn't found. */
export async function rectOf(
  page: Page,
  selector: string,
): Promise<{ x: number; y: number; w: number; h: number } | undefined> {
  const box = await page.locator(selector).first().boundingBox().catch(() => null);
  if (!box) return undefined;
  return { x: box.x * DSF, y: box.y * DSF, w: box.width * DSF, h: box.height * DSF };
}

/** Convert a Playwright bounding box (CSS px) to a 2x-pixel focus rect. */
export function focusFromBox(box: { x: number; y: number; width: number; height: number }): {
  x: number;
  y: number;
  w: number;
  h: number;
} {
  return { x: box.x * DSF, y: box.y * DSF, w: box.width * DSF, h: box.height * DSF };
}

interface ShotOpts {
  caption?: string;
  hold?: number;
  cursor?: { x: number; y: number };
  click?: boolean;
  focus?: { x: number; y: number; w: number; h: number };
  transition?: Transition;
  title?: string;
  subtitle?: string;
  link?: string;
}

/**
 * Capture one beat: write a viewport PNG (unless it's a title card) and push a
 * manifest entry. `caption` persists across shots until changed; `cursor` /
 * `click` / `focus` apply only to this shot.
 */
export async function shot(page: Page, opts: ShotOpts = {}): Promise<void> {
  if (opts.caption !== undefined) pendingCaption = opts.caption;

  const n = (sceneCounters.get(currentScene) ?? 0) + 1;
  sceneCounters.set(currentScene, n);

  let image: string | undefined;
  if (!opts.title) {
    image = `${currentScene}-${String(n).padStart(2, '0')}.png`;
    const ok = await page
      .screenshot({ path: path.join(FRAMES_DIR, image) })
      .then(() => true)
      .catch(() => false);
    if (!ok) image = undefined;
  }

  recordedShots.push({
    scene: currentScene,
    index: recordedShots.length,
    image,
    caption: opts.title ? opts.caption : pendingCaption,
    hold: opts.hold ?? 2.2,
    cursor: opts.title ? undefined : opts.cursor,
    click: opts.click === true ? true : undefined,
    focus: opts.focus,
    transition: opts.transition ?? 'fade',
    title: opts.title,
    subtitle: opts.subtitle,
    link: opts.link,
  });
}

/** Flush the manifest JSON next to the frames. */
export function writeManifest(): void {
  const manifest = {
    locale: DEMO_LOCALE,
    width: SRC_WIDTH,
    height: SRC_HEIGHT,
    shots: recordedShots,
  };
  fs.writeFileSync(
    path.join(FRAMES_DIR, `manifest-${LOCALE_DIR}.json`),
    JSON.stringify(manifest, null, 2),
  );
}
