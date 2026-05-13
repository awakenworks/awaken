import { describe, expect, it } from "vitest";

import {
  applyToolSelectionMode,
  groupSelectionState,
  groupToolsBySource,
  isToolAllowed,
  isToolSelected,
  nextAllowedTools,
  nextToolSelection,
  setGroupSelection,
  toolSelectionMode,
  toolSourceFor,
  type ApiToolSource,
} from "./agent-tool-selection";

describe("agent tool selection (include variant)", () => {
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

  it("collapses to explicit null (not undefined) when every tool is selected again", () => {
    // R8 #1: a customized agent picking "all tools" must PATCH an
    // explicit null override, not DELETE the override (which would
    // re-inherit the base's restricted list).
    expect(nextAllowedTools(["search", "write"], ["search", "write"], "search", true)).toBeNull();
  });
});

describe("toolSelectionMode", () => {
  it("classifies undefined as the default 'all' mode", () => {
    expect(toolSelectionMode(undefined)).toBe("all");
  });

  it("classifies explicit null as the default 'all' mode", () => {
    expect(toolSelectionMode(null)).toBe("all");
  });

  it("treats an explicit list as 'custom', even if it lists every tool", () => {
    expect(toolSelectionMode([])).toBe("custom");
    expect(toolSelectionMode(["search"])).toBe("custom");
  });
});

describe("applyToolSelectionMode (include)", () => {
  it("returns explicit null when switching to 'all' (forces explicit override)", () => {
    expect(applyToolSelectionMode(["search"], "all", ["search", "write"])).toBeNull();
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

// R8 #1 — `excluded_tools` shares the wire shape with `allowed_tools` but
// has inverted defaults: undefined/null = "no tool excluded" (not
// "every tool excluded"). The component reuses the same helpers with a
// `variant` discriminator; these tests pin the inverted semantics.
describe("agent tool selection (exclude variant)", () => {
  const allToolIds = ["search", "write", "read"];

  it("treats undefined excluded_tools as 'no tool excluded'", () => {
    expect(isToolSelected(undefined, "search", "exclude")).toBe(false);
  });

  it("treats null excluded_tools the same as undefined", () => {
    expect(isToolSelected(null, "search", "exclude")).toBe(false);
  });

  it("treats an empty list as 'no tool excluded'", () => {
    expect(isToolSelected([], "search", "exclude")).toBe(false);
  });

  it("treats an explicit list entry as excluded", () => {
    expect(isToolSelected(["search"], "search", "exclude")).toBe(true);
    expect(isToolSelected(["search"], "read", "exclude")).toBe(false);
  });

  it("checking a tool from null adds only that tool to the excluded list", () => {
    // R8 #1: without the exclude variant, the previous helpers returned
    // undefined here — checking a tool would NOT add it to the excluded
    // list, so the UI checkbox state and the underlying value silently
    // diverged.
    expect(nextToolSelection(null, allToolIds, "search", true, "exclude")).toEqual([
      "search",
    ]);
  });

  it("checking another tool appends to the excluded list", () => {
    expect(
      nextToolSelection(["search"], allToolIds, "write", true, "exclude"),
    ).toEqual(["search", "write"]);
  });

  it("unchecking the last tool collapses to null (explicit 'block none')", () => {
    expect(
      nextToolSelection(["search"], allToolIds, "search", false, "exclude"),
    ).toBeNull();
  });

  it("unchecking a tool not in the list is a no-op", () => {
    expect(nextToolSelection(null, allToolIds, "search", false, "exclude")).toBeNull();
    expect(
      nextToolSelection(["write"], allToolIds, "search", false, "exclude"),
    ).toEqual(["write"]);
  });
});

describe("applyToolSelectionMode (exclude)", () => {
  it("custom mode from null seeds with [] (NOT every tool — that would block all tools)", () => {
    // R8 #1: this was the data-loss bug — the previous helper returned
    // `[...allToolIds]`, i.e. every tool in `excluded_tools`, so the
    // moment a user clicked "Custom exclusion" the agent lost access to
    // every tool.
    expect(applyToolSelectionMode(undefined, "custom", ["a", "b"], "exclude")).toEqual([]);
    expect(applyToolSelectionMode(null, "custom", ["a", "b"], "exclude")).toEqual([]);
  });

  it("custom mode preserves a non-empty existing list", () => {
    expect(
      applyToolSelectionMode(["a"], "custom", ["a", "b"], "exclude"),
    ).toEqual(["a"]);
  });

  it("'block none' mode emits explicit null (customized save → override)", () => {
    expect(applyToolSelectionMode(["a"], "all", ["a", "b"], "exclude")).toBeNull();
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

describe("groupSelectionState (include)", () => {
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

describe("groupSelectionState (exclude)", () => {
  const groupIds = ["a", "b"];

  it("treats undefined excluded_tools as everything UN-selected (block none)", () => {
    expect(groupSelectionState(undefined, groupIds, "exclude")).toBe("none");
  });

  it("returns 'all' when every group member is in the excluded list", () => {
    expect(groupSelectionState(["a", "b"], groupIds, "exclude")).toBe("all");
  });

  it("returns 'some' when only part of the group is excluded", () => {
    expect(groupSelectionState(["a"], groupIds, "exclude")).toBe("some");
  });
});

describe("setGroupSelection (include)", () => {
  const allToolIds = ["a", "b", "c"];

  it("adds every group tool when selecting (collapses to explicit null when complete)", () => {
    expect(setGroupSelection(["c"], allToolIds, ["a", "b"], true)).toBeNull();
  });

  it("removes every group tool when deselecting", () => {
    expect(setGroupSelection(undefined, allToolIds, ["a", "b"], false)).toEqual(["c"]);
  });

  it("collapses to explicit null when the result covers every known tool", () => {
    expect(setGroupSelection([], allToolIds, ["a", "b", "c"], true)).toBeNull();
  });

  it("ignores group ids that are not part of the catalog", () => {
    expect(setGroupSelection(["a"], allToolIds, ["a", "z"], false)).toEqual([]);
  });
});

describe("setGroupSelection (exclude)", () => {
  const allToolIds = ["a", "b", "c"];

  it("adds every group tool to the excluded list when 'Select all' is clicked", () => {
    expect(setGroupSelection(null, allToolIds, ["a", "b"], true, "exclude")).toEqual([
      "a",
      "b",
    ]);
  });

  it("collapses to explicit null when the excluded list ends up empty", () => {
    expect(
      setGroupSelection(["a"], allToolIds, ["a"], false, "exclude"),
    ).toBeNull();
  });

  it("removes only the group tools, preserving other excluded entries", () => {
    expect(
      setGroupSelection(["a", "c"], allToolIds, ["a"], false, "exclude"),
    ).toEqual(["c"]);
  });

  it("does NOT collapse when every tool is excluded — that's an intentional 'block all' state", () => {
    // Distinct from include semantics: a fully-populated allowed_tools
    // collapses to "all" because it's a redundant restriction; a fully-
    // populated excluded_tools is the meaningful "block every tool"
    // signal and must NOT collapse to null (which would mean the
    // opposite — "block none").
    expect(
      setGroupSelection([], allToolIds, ["a", "b", "c"], true, "exclude"),
    ).toEqual(["a", "b", "c"]);
  });
});
