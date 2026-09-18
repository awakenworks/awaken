import { describe, expect, it } from "vitest";
import {
  retargetBackgroundTool,
  updateBackgroundEligibility,
} from "./ToolOverridesEditor";

describe("background eligibility policy", () => {
  it("uses an idempotent exact-tool allowlist", () => {
    expect(updateBackgroundEligibility(["bash"], "bash", true)).toEqual(["bash"]);
    expect(updateBackgroundEligibility(["bash"], "mcp__docs__search", true)).toEqual([
      "bash",
      "mcp__docs__search",
    ]);
    expect(updateBackgroundEligibility(["bash"], "bash", false)).toEqual([]);
    expect(updateBackgroundEligibility(["bash"], "", true)).toEqual(["bash"]);
  });

  it("keeps background eligibility attached when a policy target is renamed", () => {
    expect(retargetBackgroundTool(["bash", "mcp__docs__search"], "bash", "mcp__docs__search"))
      .toEqual(["mcp__docs__search"]);
  });
});
