import { describe, expect, it } from "vitest";
import { readFileSync } from "node:fs";

import { patternMatches } from "./tool-catalog";

interface ParityCase {
  pattern: string;
  value: string;
  expected: boolean;
  note?: string;
}

const FIXTURE_URL = new URL(
  "../../../../crates/awaken-tool-pattern/tests/fixtures/catalog-glob-parity.json",
  import.meta.url,
);

const cases: ParityCase[] = JSON.parse(readFileSync(FIXTURE_URL, "utf8"));

describe("parity with awaken-tool-pattern (Rust)", () => {
  it("fixture has cases", () => {
    expect(cases.length).toBeGreaterThan(0);
  });

  it.each(cases)("pattern=$pattern value=$value -> $expected", (c) => {
    expect(patternMatches(c.pattern, c.value)).toBe(c.expected);
  });
});
