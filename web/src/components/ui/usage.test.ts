import { describe, expect, it } from "vitest";
import type { SessionUsage } from "../../lib/api/types";
import { usageTotal } from "./primitives";

const u = (o: Partial<SessionUsage>): SessionUsage => ({
  input_tokens: 0,
  output_tokens: 0,
  cache_read_input_tokens: 0,
  cache_creation_input_tokens: 0,
  ...o,
});

describe("usageTotal", () => {
  it("sums all four token legs", () => {
    expect(
      usageTotal(u({ input_tokens: 10, output_tokens: 5, cache_read_input_tokens: 3, cache_creation_input_tokens: 2 })),
    ).toBe(20);
  });
  it("is zero for undefined (no turn committed yet)", () => {
    expect(usageTotal(undefined)).toBe(0);
  });
});
