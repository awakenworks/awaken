import { readFileSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

import { describe, expect, it } from "vitest";

import { toolIdMatch } from "./agent-tool-selection";

interface Case {
  pattern: string;
  value: string;
  expected: boolean;
  note: string;
}

// Single source of truth shared with the Rust runtime test
// (crates/awaken-tool-pattern/tests/catalog_glob_parity.rs). Drift between
// the two matchers is what this test exists to catch.
const fixturePath = resolve(
  dirname(fileURLToPath(import.meta.url)),
  "../../../../crates/awaken-tool-pattern/tests/fixtures/catalog-glob-parity.json",
);
const cases: Case[] = JSON.parse(readFileSync(fixturePath, "utf-8"));

describe("catalog tool-id pattern parity vs awaken-tool-pattern runtime", () => {
  it.each(cases)(
    "pattern=$pattern  value=$value  expected=$expected — $note",
    ({ pattern, value, expected }) => {
      expect(toolIdMatch(pattern, value)).toBe(expected);
    },
  );
});
