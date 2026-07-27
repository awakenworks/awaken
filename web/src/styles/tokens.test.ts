import { readFileSync } from "node:fs";
import { describe, expect, it } from "vitest";

const css = readFileSync(new URL("./tokens.css", import.meta.url), "utf8");

describe("theme browser integration", () => {
  it("keeps browser-provided controls in sync with the explicit theme", () => {
    expect(css).toMatch(/:root,\s*\[data-theme="light"\]\s*\{[^}]*color-scheme:\s*light/s);
    expect(css).toMatch(/\[data-theme="dark"\]\s*\{[^}]*color-scheme:\s*dark/s);
  });
});
