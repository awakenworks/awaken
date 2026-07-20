import { describe, expect, it } from "vitest";
import { isCanonicalMcpToolId } from "./ToolOverridesEditor";

describe("isCanonicalMcpToolId", () => {
  it("accepts a runtime MCP namespace without requiring a static catalog entry", () => {
    expect(isCanonicalMcpToolId("mcp__github__create_issue")).toBe(true);
  });

  it("rejects incomplete namespaces", () => {
    expect(isCanonicalMcpToolId("mcp__github")).toBe(false);
    expect(isCanonicalMcpToolId("mcp____tool")).toBe(false);
  });
});
