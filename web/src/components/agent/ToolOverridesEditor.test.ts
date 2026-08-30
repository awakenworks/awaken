import { describe, expect, it } from "vitest";
import {
  isCanonicalMcpToolId,
  retargetBackgroundTool,
  updateBackgroundEligibility,
} from "./ToolOverridesEditor";

describe("isCanonicalMcpToolId", () => {
  it("accepts a runtime MCP namespace without requiring a static catalog entry", () => {
    expect(isCanonicalMcpToolId("mcp__github__create_issue")).toBe(true);
  });

  it("rejects incomplete namespaces", () => {
    expect(isCanonicalMcpToolId("mcp__github")).toBe(false);
    expect(isCanonicalMcpToolId("mcp____tool")).toBe(false);
  });
});

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
