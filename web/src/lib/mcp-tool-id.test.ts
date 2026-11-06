import { describe, expect, it } from "vitest";
import {
  encodeMcpComponent,
  hasMcpToolNamespace,
  isCanonicalMcpToolId,
  mcpToolId,
  mcpToolNamespace,
} from "./mcp-tool-id";

// These vectors are copied from the Rust owner's tests
// (crates/sdk/contracts/awaken-agent-contract/src/mcp_tool_id.rs). If the two
// encoders ever disagree, the console rewrites authored tool references onto
// ids the runtime never produces.
const SHARED_VECTORS: Array<[string, string]> = [
  ["calc", "calc"],
  ["awaken_session", "awaken_session"],
  ["my_server", "my_server"],
  ["Srv1", "Srv1"],
  ["123", "123"],
  ["github-mcp", "github-2dmcp"],
  ["api.example.com", "api-2eexample-2ecom"],
  ["a-b", "a-2db"],
  ["a.b", "a-2eb"],
  ["a_b", "a_b"],
  ["a__b", "a-5f_b"],
  ["my server", "my-20server"],
  ["-server-", "-2dserver-2d"],
  ["tool_\u{1F600}", "tool_-f0-9f-98-80"],
];

describe("encodeMcpComponent", () => {
  it("agrees with the Rust owner on every shared vector", () => {
    for (const [raw, encoded] of SHARED_VECTORS) {
      expect(encodeMcpComponent(raw)).toBe(encoded);
    }
  });

  it("never lets two distinct names share one namespace", () => {
    const names = ["a-b", "a.b", "a_b", "a b", "a__b", "-a-b-", "a-2db"];
    const namespaces = new Set(names.map((name) => mcpToolNamespace(name)));
    expect(namespaces.size).toBe(names.length);
  });

  it("never emits the separator inside a component", () => {
    for (const [raw] of SHARED_VECTORS) {
      expect(encodeMcpComponent(raw)).not.toContain("__");
    }
  });
});

describe("mcpToolId", () => {
  it("builds the id the runtime producer builds", () => {
    expect(mcpToolId("calc", "add")).toBe("mcp__calc__add");
    expect(mcpToolId("github-mcp", "search")).toBe("mcp__github-2dmcp__search");
  });

  it("has no identity for an empty half", () => {
    expect(mcpToolId("", "search")).toBeNull();
    expect(mcpToolId("srv", "")).toBeNull();
    expect(mcpToolNamespace("")).toBeNull();
  });
});

describe("namespace recognition", () => {
  it("accepts a runtime MCP namespace without requiring a static catalog entry", () => {
    expect(isCanonicalMcpToolId("mcp__github__create_issue")).toBe(true);
  });

  it("rejects incomplete namespaces", () => {
    expect(isCanonicalMcpToolId("mcp__github")).toBe(false);
    expect(isCanonicalMcpToolId("mcp____tool")).toBe(false);
  });

  it("separates membership of the namespace from being a complete id", () => {
    expect(hasMcpToolNamespace("mcp__github")).toBe(true);
    expect(isCanonicalMcpToolId("mcp__github")).toBe(false);
    expect(hasMcpToolNamespace("bash")).toBe(false);
  });
});
