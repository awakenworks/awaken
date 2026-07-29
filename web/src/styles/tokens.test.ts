import { readFileSync } from "node:fs";
import { describe, expect, it } from "vitest";

const css = readFileSync(new URL("./tokens.css", import.meta.url), "utf8");
const base = readFileSync(new URL("./base.css", import.meta.url), "utf8");

describe("theme browser integration", () => {
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
});
