import { describe, expect, it } from "vitest";

import {
  applyToolSelectionMode,
  groupSelectionState,
  groupToolsBySource,
  isToolAllowed,
  nextAllowedTools,
  setGroupSelection,
  toolSelectionMode,
  toolSourceFor,
  type ApiToolSource,
} from "./agent-tool-selection";

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

describe("toolSelectionMode", () => {
  it("classifies undefined as the default 'all' mode", () => {
    expect(toolSelectionMode(undefined)).toBe("all");
  });

  it("treats an explicit list as 'custom', even if it lists every tool", () => {
    expect(toolSelectionMode([])).toBe("custom");
    expect(toolSelectionMode(["search"])).toBe("custom");
  });
});

describe("applyToolSelectionMode", () => {
  it("clears the explicit list when switching back to 'all'", () => {
    expect(applyToolSelectionMode(["search"], "all", ["search", "write"])).toBeUndefined();
  });

  it("preserves the existing custom list when re-entering custom mode", () => {
    expect(applyToolSelectionMode(["search"], "custom", ["search", "write"])).toEqual([
      "search",
    ]);
  });

  it("seeds custom mode with every known tool when there is no prior list", () => {
    expect(applyToolSelectionMode(undefined, "custom", ["a", "b"])).toEqual(["a", "b"]);
  });
});

describe("toolSourceFor", () => {
  it("recognises mcp:* tools and extracts the server id", () => {
    expect(toolSourceFor("mcp:weather/forecast")).toMatchObject({
      kind: "mcp",
      key: "mcp:weather",
      label: "MCP · weather",
    });
  });

  it("recognises mcp:* with no server suffix", () => {
    expect(toolSourceFor("mcp:")).toMatchObject({ kind: "mcp", label: "MCP" });
  });

  it("recognises plugin:* tools and extracts the plugin id", () => {
    expect(toolSourceFor("plugin:reminder/add")).toMatchObject({
      kind: "plugin",
      key: "plugin:reminder",
      label: "Plugin · reminder",
    });
  });

  it("treats other tool ids as built-ins", () => {
    expect(toolSourceFor("Bash")).toMatchObject({
      kind: "builtin",
      key: "builtin",
    });
  });

  it("uses explicit mcp source from backend over id inference", () => {
    const apiSource: ApiToolSource = { kind: "mcp", id: "weather" };
    expect(toolSourceFor("mcp__weather__forecast", apiSource)).toMatchObject({
      kind: "mcp",
      key: "mcp:weather",
      label: "MCP · weather",
    });
  });

  it("uses explicit plugin source from backend over id inference", () => {
    const apiSource: ApiToolSource = { kind: "plugin", id: "reminder" };
    expect(toolSourceFor("some-tool-id", apiSource)).toMatchObject({
      kind: "plugin",
      key: "plugin:reminder",
      label: "Plugin · reminder",
    });
  });

  it("uses explicit builtin source from backend", () => {
    const apiSource: ApiToolSource = { kind: "builtin" };
    expect(toolSourceFor("Bash", apiSource)).toMatchObject({
      kind: "builtin",
      key: "builtin",
      label: "Built-in",
    });
  });

  it("handles mcp source with no id gracefully", () => {
    const apiSource: ApiToolSource = { kind: "mcp" };
    expect(toolSourceFor("mcp__x__y", apiSource)).toMatchObject({
      kind: "mcp",
      label: "MCP",
      key: "mcp:",
    });
  });
});

describe("groupToolsBySource", () => {
  it("groups by source and orders builtin → plugin → mcp", () => {
    const groups = groupToolsBySource([
      { id: "mcp:weather/forecast" },
      { id: "Read" },
      { id: "plugin:reminder/add" },
      { id: "Bash" },
      { id: "mcp:weather/now" },
      { id: "mcp:db/query" },
    ]);
    expect(groups.map((g) => g.source.key)).toEqual([
      "builtin",
      "plugin:reminder",
      "mcp:db",
      "mcp:weather",
    ]);
    const builtin = groups[0];
    expect(builtin.tools.map((t) => t.id)).toEqual(["Bash", "Read"]);
    const weather = groups[3];
    expect(weather.tools.map((t) => t.id)).toEqual([
      "mcp:weather/forecast",
      "mcp:weather/now",
    ]);
  });

  it("uses explicit source field when present, ignoring id prefix", () => {
    const groups = groupToolsBySource([
      { id: "mcp__weather__forecast", source: { kind: "mcp" as const, id: "weather" } },
      { id: "some-tool", source: { kind: "plugin" as const, id: "reminder" } },
      { id: "Bash", source: { kind: "builtin" as const } },
    ]);
    expect(groups.map((g) => g.source.key)).toEqual([
      "builtin",
      "plugin:reminder",
      "mcp:weather",
    ]);
    expect(groups[0].tools.map((t) => t.id)).toEqual(["Bash"]);
    expect(groups[1].tools.map((t) => t.id)).toEqual(["some-tool"]);
    expect(groups[2].tools.map((t) => t.id)).toEqual(["mcp__weather__forecast"]);
  });
});

describe("groupSelectionState", () => {
  const groupIds = ["a", "b"];

  it("treats undefined allowed_tools as everything selected", () => {
    expect(groupSelectionState(undefined, groupIds)).toBe("all");
  });

  it("returns 'none' when no group member is in the allowed list", () => {
    expect(groupSelectionState(["x"], groupIds)).toBe("none");
  });

  it("returns 'some' when only part of the group is selected", () => {
    expect(groupSelectionState(["a"], groupIds)).toBe("some");
  });
});

describe("setGroupSelection", () => {
  const allToolIds = ["a", "b", "c"];

  it("adds every group tool when selecting", () => {
    expect(setGroupSelection(["c"], allToolIds, ["a", "b"], true)).toBeUndefined();
  });

  it("removes every group tool when deselecting", () => {
    expect(setGroupSelection(undefined, allToolIds, ["a", "b"], false)).toEqual(["c"]);
  });

  it("collapses to undefined when the result covers every known tool", () => {
    expect(setGroupSelection([], allToolIds, ["a", "b", "c"], true)).toBeUndefined();
  });

  it("ignores group ids that are not part of the catalog", () => {
    expect(setGroupSelection(["a"], allToolIds, ["a", "z"], false)).toEqual([]);
  });
});
