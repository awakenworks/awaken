import { readFileSync } from "node:fs";
import { describe, expect, it } from "vitest";

const css = readFileSync(new URL("./tokens.css", import.meta.url), "utf8");
const base = readFileSync(new URL("./base.css", import.meta.url), "utf8");
const html = readFileSync(new URL("../../index.html", import.meta.url), "utf8");
const chrome = readFileSync(new URL("../components/app/TopChrome.tsx", import.meta.url), "utf8");
const favicon = readFileSync(new URL("../../public/favicon.svg", import.meta.url), "utf8");

describe("theme browser integration", () => {
  it("uses the awakenworks.com Agents watermetal and teal identity", () => {
    expect(css).toMatch(/\[data-theme="light"\][\s\S]*?--canvas:\s*#eef1f3[\s\S]*?--accent:\s*#0f7d8c/);
    expect(css).toMatch(/\[data-theme="dark"\][\s\S]*?--canvas:\s*#0e171c[\s\S]*?--accent:\s*#3fb3c2/);
    expect(css).not.toMatch(/--accent:[^;]*\b(?:265|263|267)\b/);
  });

  it("uses the canonical Awaken Agents mark in chrome and favicon", () => {
    const geometry = /M14\.2 6h3\.6l8\.6 20h-3L16 8\.4 8\.6 26h-3Z/;
    for (const source of [chrome, favicon]) {
      expect(source).toMatch(geometry);
      expect(source).toMatch(/cx="16" cy="20\.2" r="2\.5"/);
      expect(source).not.toMatch(/<span>A<\/span>|Platform|Harness/);
    }
    expect(base).toMatch(/\.awaken-mark\s*\{[^}]*--mark-primary:\s*#0f6f7b[^}]*--mark-decision:\s*#5c43b2/s);
    expect(base).toMatch(/\[data-theme="dark"\] \.awaken-mark\s*\{[^}]*--mark-primary:\s*#68ced9[^}]*--mark-decision:\s*#a392ff/s);
    expect(html).toMatch(/<title>Awaken Agents<\/title>/);
    expect(html).toMatch(/href="\/favicon\.svg"/);
  });

  it("keeps browser-provided controls in sync with the explicit theme", () => {
    expect(css).toMatch(/:root,\s*\[data-theme="light"\]\s*\{[^}]*color-scheme:\s*light/s);
    expect(css).toMatch(/\[data-theme="dark"\]\s*\{[^}]*color-scheme:\s*dark/s);
  });

  // Cause → effect → assertions:
  // Dense mono UI plus dark-mode faint text made actionable help look disabled →
  // readable copy uses the soft tier at a 12px floor, while the body keeps the
  // same compact information architecture at a legible 14px base.
  it("reserves faint text for metadata and keeps readable copy on the soft tier", () => {
    expect(base).toMatch(/body\s*\{[^}]*font-size:\s*14px[^}]*text-rendering:\s*optimizeLegibility/s);
    expect(base).toMatch(/\.field\s*>\s*label\s*\{[^}]*font-size:\s*12px[^}]*color:\s*var\(--fg2\)/s);
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
    expect(css).toMatch(/--font-sans:[^;]+ui-sans-serif[^;]+system-ui/);
    expect(css).toMatch(/--font-mono:[^;]+ui-monospace/);
  });

  // Native SVG dimensions default to 300x150. Without an explicit shared
  // constraint, the compact mobile search control visually covers the brand
  // even though the page itself reports no horizontal overflow.
  it("bounds topbar icons and preserves comfortable mobile actions", () => {
    expect(base).toMatch(/\.search-box svg,[^{]+\.chrome-btn svg\s*\{[^}]*width:\s*16px[^}]*height:\s*16px/s);
    expect(base).toMatch(/@media \(max-width:\s*520px\)[\s\S]*?\.btn\s*\{[^}]*min-height:\s*36px/s);
    expect(base).toMatch(/@media \(max-width:\s*520px\)[\s\S]*?\.chrome-btn\s*\{[^}]*width:\s*40px[^}]*height:\s*40px/s);
    expect(base).toMatch(/@media \(max-width:\s*520px\)[\s\S]*?input:not\(\[type="checkbox"\]\):not\(\[type="radio"\]\)[\s\S]*?min-height:\s*40px/s);
    expect(base).toMatch(/@media \(max-width:\s*520px\)[\s\S]*?select\.input,[\s\S]*?min-height:\s*40px/s);
    expect(base).toMatch(/\.empty-inline a\s*\{[^}]*min-height:\s*40px/s);
    expect(base).toMatch(/\.empty-inline a\s*\{[^}]*display:\s*inline-flex[^}]*min-height:\s*32px/s);
    expect(base).toMatch(/\.ui-data-grid__search\.input\s*\{[^}]*min-width:\s*0[^}]*max-width:\s*100%/s);
    expect(base).toMatch(/\.main\s*\{[^}]*min-width:\s*0[^}]*min-height:\s*0/s);
    expect(base).toMatch(/\.card:has\(> \.table\),[\s\S]*?\{[^}]*overflow-x:\s*auto !important/s);
    expect(base).toMatch(/@media \(max-width:\s*760px\)[\s\S]*?\.mobile-nav\s*\{[^}]*display:\s*grid/s);
    expect(base).toMatch(/@media \(max-width:\s*760px\)[\s\S]*?\.assistant-fab,[\s\S]*?\.assistant-fab-panel\s*\{[^}]*display:\s*none/s);
    expect(base).toMatch(/@media \(max-width:\s*760px\)[\s\S]*?\.responsive-table-card \.table td\[colspan\]\s*\{[^}]*display:\s*block/s);
    expect(base).toMatch(/@media \(max-width:\s*980px\)[\s\S]*?\.page-purpose\s*\{[^}]*grid-template-columns:\s*1fr/s);
    expect(base).toMatch(/@media \(max-width:\s*980px\)[\s\S]*?\.btn\s*\{[^}]*min-height:\s*32px/s);
    expect(base).toMatch(/\.manage-link\s*\{[^}]*min-height:\s*32px/s);
  });
});
