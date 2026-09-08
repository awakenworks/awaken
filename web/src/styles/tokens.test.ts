import { renderAdaptiveFaviconSvg } from "@awaken/ui/brand";
import { brandFaviconPlugin } from "@awaken/ui/brand/vite";
import { readFileSync } from "node:fs";
import { describe, expect, it } from "vitest";

const css = readFileSync(new URL("./tokens.css", import.meta.url), "utf8");
const family = readFileSync(new URL("../../node_modules/@awaken/ui/dist/styles/family.css", import.meta.url), "utf8");
const main = readFileSync(new URL("../main.tsx", import.meta.url), "utf8");
const appState = readFileSync(new URL("../lib/app-state.tsx", import.meta.url), "utf8");
const base = readFileSync(new URL("./base.css", import.meta.url), "utf8");
const sharedForms = readFileSync(new URL("../../node_modules/@awaken/ui/dist/styles/components-forms-data.css", import.meta.url), "utf8");
const sharedCore = readFileSync(new URL("../../node_modules/@awaken/ui/dist/styles/components-core.css", import.meta.url), "utf8");
const sharedContract = readFileSync(new URL("../../node_modules/@awaken/ui/dist/styles/contract.css", import.meta.url), "utf8");
const html = readFileSync(new URL("../../index.html", import.meta.url), "utf8");
const chrome = readFileSync(new URL("../components/app/TopChrome.tsx", import.meta.url), "utf8");
const vite = readFileSync(new URL("../../vite.config.ts", import.meta.url), "utf8");

describe("theme browser integration", () => {
  it("maps Agents vocabulary to the shared family owner", () => {
    // P1 existing product aliases -> shared values; no copied palette or font stack.
    // P2 cold load -> shared prepaint bootstrap, with the old preference migrated.
    expect(css).toContain("--canvas: var(--aw-canvas)");
    expect(css).toContain("--accent: var(--aw-accent)");
    expect(css).not.toMatch(/#[a-f0-9]{3,8}\b/i);
    expect(main).toContain('"@awaken/ui/brand/family.css"');
    expect(vite).toContain('brandAppearancePlugin("agents", { legacyStorageKeys: ["awaken.console.theme"] })');
    expect(appState).not.toMatch(/awaken\.console\.theme|setTheme|data-theme/);
    expect(chrome).toContain("<AppearanceSelect labels=");
  });

  it("uses the canonical Awaken Agents mark in chrome and favicon", () => {
    // R1 every product entrance -> the shared brand renderer; R2 build/dev ->
    // canonical inline favicon, no separately editable geometry or file request.
    expect(chrome).toContain('from "@awaken/ui/brand/react"');
    expect(chrome).toContain('<BrandMark mark="agents"');
    expect(chrome).toContain('<BrandMark mark="workforce"');
    expect(chrome).not.toMatch(/data-silhouette|function AwakenMark/);
    expect(base).not.toContain("--mark-primary:");
    expect(vite).toContain('brandFaviconPlugin("agents")');
    const href = brandFaviconPlugin("agents").transformIndexHtml()[0]!.attrs.href;
    expect(decodeURIComponent(href.split(",")[1]!)).toBe(renderAdaptiveFaviconSvg("agents"));
    expect(html).toMatch(/<title>Awaken Agents<\/title>/);
    expect(html).not.toMatch(/rel="icon"/);
  });

  it("delegates suite interaction to the one shared component", () => {
    // Duplication boundary: the product supplies copy, icons and exact
    // Cloud-projected URLs, while @awaken/ui owns menu DOM and keyboard/focus
    // behavior. A local `.suite-menu` recipe would reintroduce a second owner.
    expect(chrome).toMatch(/import \{[^}]*SuiteSwitcher[^}]*\} from "@awaken\/ui"/s);
    // N7 copy changes to Workforce while the deployed sibling intent stays flow.
    expect(chrome).toContain('id: "flow", label: "Awaken Workforce"');
    expect(chrome).toContain('suiteProductEntryUrl(hubUrl, "flow")');
    expect(chrome).toContain("/usage-billing");
    expect(chrome).toContain("/settings");
    expect(chrome).toContain("/logout");
    expect(base).not.toMatch(/\.suite-menu/);
  });

  it("delegates native control color-scheme to the family theme", () => {
    expect(family).toContain('[data-theme="light"] { color-scheme: light; }');
    expect(family).toContain('[data-theme="dark"] { color-scheme: dark; }');
  });

  // Cause → effect → assertions:
  // Dense mono UI plus dark-mode faint text made actionable help look disabled →
  // readable copy uses the soft tier at a 12px floor, while the body keeps the
  // same compact information architecture at a legible 14px base.
  it("reserves faint text for metadata and keeps readable copy on the soft tier", () => {
    expect(base).toMatch(/body\s*\{[^}]*font-size:\s*14px[^}]*text-rendering:\s*optimizeLegibility/s);
    // The shared recipe owns label typography; the product supplies color tokens.
    expect(sharedForms).toContain("font-size: var(--ui-font-size-xs)");
    expect(sharedContract).toContain("--ui-font-size-xs: 0.75rem");
    expect(base).toMatch(/\.readiness-item small,[^{]+settings-link small\s*\{[^}]*color:\s*var\(--fg2\)[^}]*font-size:\s*12px/s);
    expect(base).toMatch(/\.mut\s*\{[^}]*color:\s*var\(--fg2\)[^}]*font-size:\s*12\.5px/s);
  });

  /**
   * Offline runtime decision table.
   * Causes: C1 the console loads online or offline; C2 the host has or lacks the
   * preferred local mono font. Effects: E1 no remote font request or privacy
   * side effect occurs; E2 the token-owned system fallback renders immediately.
   * Rules: R1 C1(any)+C2(present) -> preferred local face; R2 C1(any)+C2(absent)
   * -> ui-monospace fallback. Both rules require E1 and never block the UI.
   */
  it("keeps the production entrypoint free of remote font dependencies", () => {
    expect(html).not.toMatch(/fonts\.(googleapis|gstatic)\.com/);
    expect(html).not.toMatch(/<link[^>]+rel=["']stylesheet["'][^>]+https?:/);
    expect(css).toContain("--font-sans: var(--aw-font-body)");
    expect(family).toMatch(/--aw-font-body:[^;]+ui-sans-serif[^;]+system-ui/);
    expect(css).toContain("--font-mono: var(--aw-font-mono)");
    expect(family).toMatch(/--aw-font-mono:[^;]+ui-monospace/);
  });

  // Native SVG dimensions default to 300x150. Without an explicit shared
  // constraint, the compact mobile search control visually covers the brand
  // even though the page itself reports no horizontal overflow.
  it("bounds topbar icons and preserves comfortable mobile actions", () => {
    expect(base).toMatch(/\.search-box svg,[^{]+\.chrome-btn svg\s*\{[^}]*width:\s*16px[^}]*height:\s*16px/s);
    expect(base).toMatch(/\.empty-inline a\s*\{[^}]*display:\s*inline-flex[^}]*min-height:\s*32px/s);
    // Density rules: fine/coarse pointer -> shared 34/44px medium controls at
    // every viewport. Consumer breakpoints may place controls but cannot replace
    // the authority with 32/36/40px values. Browser tests check resolved metrics.
    expect(base).not.toMatch(/--ui-control-height-(?:sm|md|lg)\s*:/);
    expect(base).toContain("width: var(--ui-control-height-md); height: var(--ui-control-height-md)");
    expect(base).toContain("min-height: var(--ui-control-height-md)");
    expect(sharedCore).toMatch(/\.ui-data-grid__search\s*\{[^}]*min-width:\s*0[^}]*max-width:\s*100%/s);
    expect(base).toMatch(/\.main\s*\{[^}]*min-width:\s*0[^}]*min-height:\s*0/s);
    expect(base).toMatch(/\.ui-card:has\(> \.ui-data-grid__table\),[\s\S]*?\{[^}]*overflow-x:\s*auto !important/s);
    expect(base).toMatch(/@media \(max-width:\s*760px\)[\s\S]*?\.mobile-nav\s*\{[^}]*display:\s*grid/s);
    expect(base).toMatch(/@media \(max-width:\s*760px\)[\s\S]*?\.assistant-fab,[\s\S]*?\.assistant-fab-panel\s*\{[^}]*display:\s*none/s);
    expect(base).toMatch(/@media \(max-width:\s*760px\)[\s\S]*?\.responsive-table-card \.ui-data-grid__table td\[colspan\]\s*\{[^}]*display:\s*block/s);
    expect(base).toMatch(/@media \(max-width:\s*980px\)[\s\S]*?\.page-purpose\s*\{[^}]*grid-template-columns:\s*1fr/s);
    expect(base).toMatch(/\.manage-link\s*\{[^}]*min-height:\s*32px/s);
  });
});
