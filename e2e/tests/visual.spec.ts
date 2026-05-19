import { test, expect, type Page } from '@playwright/test';

/* Visual regression baseline.
 *
 * One snapshot per (surface × theme [× locale]) combo. Purpose:
 *   - Catch unintended pixel drift from brand-token edits, CSS sweeps,
 *     dependency bumps, or upstream Starlight / Astro upgrades.
 *   - Lock the achromatic + mono + 2px brand identity per the
 *     awaken-admin / docs-styling / www-landing specs.
 *
 * `maxDiffPixelRatio: 0.02` absorbs sub-pixel anti-alias differences;
 * any larger drift fails CI.
 *
 * Coverage:
 *   admin · dashboard / agents list / agent editor             × light + dark
 *   docs  · landing                                            × light + dark × en + zh-cn
 *   docs  · reference (sidebar + code block)                   × light + dark
 *   docs  · explanation (mermaid diagram render)               × light + dark
 *
 * Naming: `<surface>-<theme>[-locale].png`. Files land at
 * `tests/visual.spec.ts-snapshots/`.
 */

const ADMIN = 'http://127.0.0.1:3002';
const DOCS = 'http://127.0.0.1:3003';

/* Must match the token the visual-config webServer launches with. */
const TEST_ADMIN_TOKEN = 'test-bearer-visual';

async function setTheme(page: Page, theme: 'light' | 'dark') {
  /* Storage keys must match what each app actually reads:
   *   admin-console: `awaken.admin.theme` (dot-separated, see use-theme.ts)
   *   Starlight:     `starlight-theme`
   * The old `awaken-admin-theme` (hyphen) was a typo — admin ignored it
   * silently so dark snapshots regressed to light without warning. */
  await page.evaluate((t) => {
    document.documentElement.setAttribute('data-theme', t);
    try { localStorage.setItem('awaken.admin.theme', t); } catch {}
    try { localStorage.setItem('starlight-theme', t); } catch {}
  }, theme);
  /* Hard-assert the attribute actually flipped before we screenshot, so a
   * future storage-key drift would fail loudly here instead of silently
   * producing light pixels under a `*-dark.png` filename. */
  await expect
    .poll(() => page.evaluate(() => document.documentElement.getAttribute('data-theme')))
    .toBe(theme);
}

async function settle(page: Page) {
  await page.evaluate(() => {
    document.querySelectorAll<HTMLElement>('[class*=animate-]').forEach((el) => {
      el.style.animation = 'none';
    });
  });
  await page.waitForLoadState('networkidle');
}

/* ---------- admin-console ---------- */

test.describe('admin-console visual baseline', () => {
  /* Admin requires a bearer token in localStorage before any /v1/config
   * call resolves; otherwise the token modal pops and screenshots
   * capture the modal instead of the real chrome. addInitScript runs
   * BEFORE the page script that reads from localStorage. */
  test.beforeEach(async ({ page }) => {
    await page.addInitScript((token) => {
      try { localStorage.setItem('awaken.adminToken', token); } catch {}
    }, TEST_ADMIN_TOKEN);
  });

  for (const theme of ['light', 'dark'] as const) {
    test(`dashboard · ${theme}`, async ({ page }) => {
      await page.goto(`${ADMIN}/`);
      await setTheme(page, theme);
      await page.waitForLoadState('networkidle');
      await settle(page);
      await expect(page).toHaveScreenshot(`admin-dashboard-${theme}.png`);
    });

    test(`agents list · ${theme}`, async ({ page }) => {
      await page.goto(`${ADMIN}/agents`);
      await setTheme(page, theme);
      await page.waitForLoadState('networkidle');
      await settle(page);
      await expect(page).toHaveScreenshot(`admin-agents-${theme}.png`);
    });

    test(`agent editor · ${theme}`, async ({ page }) => {
      /* `a2a` is one of the 43 seed entities; deep-link straight into
       * the editor so we capture the form chrome, sticky save bar,
       * eyebrow + h1, and tabbed panels in one shot. */
      await page.goto(`${ADMIN}/agents/a2a`);
      await setTheme(page, theme);
      await page.waitForLoadState('networkidle');
      await settle(page);
      await expect(page).toHaveScreenshot(`admin-agent-editor-${theme}.png`);
    });
  }
});

/* ---------- docs landing ---------- */

test.describe('docs landing visual baseline', () => {
  for (const theme of ['light', 'dark'] as const) {
    for (const locale of ['', '/zh-cn'] as const) {
      const localeTag = locale ? 'zh-cn' : 'en';
      test(`landing · ${theme} · ${localeTag}`, async ({ page }) => {
        await page.goto(`${DOCS}/awaken${locale}/`);
        await setTheme(page, theme);
        await page.waitForLoadState('networkidle');
        await settle(page);
        await expect(page).toHaveScreenshot(
          `docs-landing-${theme}-${localeTag}.png`,
        );
      });
    }
  }
});

/* ---------- docs body chrome (sidebar + code + headings) ---------- */

test.describe('docs page visual baseline', () => {
  for (const theme of ['light', 'dark'] as const) {
    test(`reference page · ${theme}`, async ({ page }) => {
      /* `reference/tool-trait` has Starlight sidebar (groups + active item),
       * TOC, multiple H2/H3 headings, rust code blocks with Shiki theme,
       * and kbd/inline-code samples — covers the bulk of docs-styling
       * spec surface in one page. */
      await page.goto(`${DOCS}/awaken/reference/tool-trait/`);
      await setTheme(page, theme);
      await page.waitForLoadState('networkidle');
      await settle(page);
      await expect(page).toHaveScreenshot(`docs-reference-${theme}.png`);
    });

    test(`mermaid diagram · ${theme}`, async ({ page }) => {
      /* `explanation/architecture` renders the system Mermaid diagram.
       * The diagram lives below the fold (after intro + text code block),
       * so a viewport screenshot from the page top misses it entirely.
       * Snapshot the locator instead so we lock down (a) Mermaid actually
       * renders to SVG, (b) the SVG honours the brand themeCSS in both
       * themes. */
      await page.goto(`${DOCS}/awaken/explanation/architecture/`);
      await setTheme(page, theme);
      await page.waitForLoadState('networkidle');
      const diagram = page.locator('.mermaid svg').first();
      /* astro-mermaid swaps `<pre class="mermaid">` for an SVG after
       * hydration — wait until the SVG is attached + visible. */
      await expect(diagram).toBeVisible({ timeout: 15_000 });
      await diagram.scrollIntoViewIfNeeded();
      await settle(page);
      await expect(diagram).toHaveScreenshot(`docs-mermaid-${theme}.png`);
    });
  }
});
