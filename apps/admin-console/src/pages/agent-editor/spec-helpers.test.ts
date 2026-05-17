import { describe, expect, it } from "vitest";
import { jsonSemanticallyEqual } from "./spec-helpers";

describe("agent spec helpers", () => {
  it("compares JSON objects with stable key ordering", () => {
    expect(
      jsonSemanticallyEqual(
        { sections: { beta: 2, alpha: 1 } },
        { sections: { alpha: 1, beta: 2 } },
      ),
    ).toBe(true);
  });
});
