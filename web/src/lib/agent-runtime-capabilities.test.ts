import { describe, expect, it } from "vitest";
import { acpWorkingDirectoryIssue, runtimeCapabilitySupport } from "./agent-runtime-capabilities";

describe("Agent execution capability matrix", () => {
  it("keeps Managed Agent platform capabilities independent from the selected Harness", () => {
    const native = runtimeCapabilitySupport(false);
    const acp = runtimeCapabilitySupport(true);
    expect(native.environment_session).toBe("supported");
    expect(acp.environment_session).toBe("supported");
    expect(native.context).toBe("supported");
    expect(acp.context).toBe("supported");
  });

  it("narrows only capabilities that cross the external Harness boundary", () => {
    const native = runtimeCapabilitySupport(false);
    const acp = runtimeCapabilitySupport(true);
    expect(native.tools_mcp).toBe("supported");
    expect(acp.tools_mcp).toBe("conditional");
    expect(native.state_concurrency).toBe("supported");
    expect(acp.state_concurrency).toBe("unavailable");
    expect(native.background_tools).toBe("supported");
    expect(acp.background_tools).toBe("unavailable");
  });

  it("uses the server runtime profile instead of inferring support from ACP alone", () => {
    const acp = runtimeCapabilitySupport(true, {
      awaken_tool_bridge: "unavailable",
      state_machine: "unavailable",
      background_tools: "unavailable",
    });
    expect(acp.tools_mcp).toBe("unavailable");
  });
});

describe("ACP working directory guidance", () => {
  it("accepts only clean Session-relative paths", () => {
    expect(acpWorkingDirectoryIssue("repo/src")).toBeNull();
    expect(acpWorkingDirectoryIssue("/tmp/repo")).toBe("absolute");
    expect(acpWorkingDirectoryIssue("C:\\repo")).toBe("absolute");
    expect(acpWorkingDirectoryIssue("repo\\src")).toBe("backslash");
    expect(acpWorkingDirectoryIssue("repo:src")).toBe("colon");
    expect(acpWorkingDirectoryIssue("repo/../secret")).toBe("traversal");
    expect(acpWorkingDirectoryIssue("repo//src")).toBe("empty_segment");
    expect(acpWorkingDirectoryIssue("x".repeat(513))).toBe("too_long");
  });
});
