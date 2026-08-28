import { describe, expect, it } from "vitest";
import type { SessionUsage } from "../../lib/api/types";
import { usageTotal } from "./primitives";

const u = (o: Partial<SessionUsage>): SessionUsage => ({
  input_tokens: 0,
  output_tokens: 0,
  cache_read_input_tokens: 0,
  ...o,
});

describe("usageTotal", () => {
  /**
   * Cause/effect rule U1: official usage contains input/output, cache read, and
   * zero/one/both cache-creation lifetimes -> the badge sums every populated
   * leg exactly once; absent optional counters contribute zero.
   */
  it("sums all official token legs and cache lifetimes", () => {
    expect(
      usageTotal(u({
        input_tokens: 10,
        output_tokens: 5,
        cache_read_input_tokens: 3,
        cache_creation: {
          ephemeral_1h_input_tokens: 2,
          ephemeral_5m_input_tokens: 4,
        },
      })),
    ).toBe(24);
  });
  /** Cause/effect rule U2: no committed usage -> a stable zero total. */
  it("is zero for undefined (no turn committed yet)", () => {
    expect(usageTotal(undefined)).toBe(0);
  });
});
