import { describe, expect, it } from "vitest";

import { isToolAllowed, nextAllowedTools } from "./agent-tool-selection";

describe("agent tool selection", () => {
  const allToolIds = ["search", "write", "read"];

  it("treats undefined allowed_tools as unrestricted", () => {
    expect(isToolAllowed(undefined, "search")).toBe(true);
  });

  it("treats an empty allowed_tools list as no tools selected", () => {
    expect(isToolAllowed([], "search")).toBe(false);
  });

  it("starts from all tools when removing one from the unrestricted state", () => {
    expect(nextAllowedTools(undefined, allToolIds, "write", false)).toEqual([
      "search",
      "read",
    ]);
  });

  it("can re-add a tool after the user removed all selections", () => {
    expect(nextAllowedTools([], allToolIds, "read", true)).toEqual(["read"]);
  });

  it("collapses back to undefined when every tool is selected again", () => {
    expect(nextAllowedTools(["search", "write"], ["search", "write"], "search", true)).toBe(
      undefined,
    );
  });
});
