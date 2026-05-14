import { describe, expect, it } from "vitest";

import {
  applyToolSelectionMode,
  groupSelectionState,
  groupToolsBySource,
  isExplicitAll,
  isLegacyCatalogValue,
  isToolAllowed,
  isToolSelectionPatternBacked,
  nextAllowedTools,
  setGroupSelection,
  toolSelectionMode,
  toolSelectionPattern,
  toolSourceFor,
  type ApiToolSource,
} from "./agent-tool-selection";

describe("isToolAllowed", () => {
  it("treats undefined as legacy implicit allow-all", () => {
    expect(isToolAllowed(undefined, "search")).toBe(true);
  });

  it("treats null as legacy implicit allow-all", () => {
    expect(isToolAllowed(null, "search")).toBe(true);
  });

  it("treats null as legacy block-none for the exclude variant", () => {
    expect(isToolAllowed(null, "search", "exclude")).toBe(false);
  });

  it("treats ['*'] as explicit allow-all", () => {
    expect(isToolAllowed(["*"], "search")).toBe(true);
    expect(isToolAllowed(["*"], "mcp__github__pr")).toBe(true);
  });

  it("treats an empty allowed_tools list as no tools selected", () => {
    expect(isToolAllowed([], "search")).toBe(false);
  });

  it("matches literal tool id entries", () => {
    expect(isToolAllowed(["search", "read"], "search")).toBe(true);
    expect(isToolAllowed(["search", "read"], "write")).toBe(false);
  });

  it("matches glob entries", () => {
    expect(isToolAllowed(["mcp__github__*"], "mcp__github__pr")).toBe(true);
    expect(isToolAllowed(["mcp__github__*"], "mcp__gitlab__pr")).toBe(false);
    expect(isToolAllowed(["read_*"], "read_file")).toBe(true);
  });

  it("uses catalog tool-id wildcard semantics", () => {
    // '*' matches any sequence, including '/', ':' and '_'
    expect(isToolAllowed(["*"], "Bash")).toBe(true);
    expect(isToolAllowed(["*"], "mcp:weather/forecast")).toBe(true);
    expect(isToolAllowed(["tool/id/*"], "tool/id/read")).toBe(true);
    expect(isToolAllowed(["tool/id/*"], "tool/id/read/nested")).toBe(true);
    expect(isToolAllowed(["mcp:*"], "mcp:weather/forecast")).toBe(true);
    expect(isToolAllowed(["mcp:*"], "plugin:reminder/add")).toBe(false);
    expect(isToolAllowed(["*issue"], "mcp__github__read_issue")).toBe(true);

    // Multiple consecutive '*'s are still just wildcards (no path-glob semantics).
    expect(isToolAllowed(["**"], "tool/id/read")).toBe(true);

    // Path-glob, regex, arg-syntax, and leading-! are *not* catalog features;
    // their characters are matched literally.
    expect(isToolAllowed(["mcp__github__read?"], "mcp__github__read1")).toBe(false);
    expect(isToolAllowed(["mcp__github__read?"], "mcp__github__read?")).toBe(true);
    expect(isToolAllowed(["mcp__[ab]*"], "mcp__a_tool")).toBe(false);
    expect(isToolAllowed(["mcp__{github,gitlab}__*"], "mcp__gitlab__issue")).toBe(false);
    expect(isToolAllowed(["!Bash"], "Read")).toBe(false);
    expect(isToolAllowed(["!Bash"], "!Bash")).toBe(true);
    expect(isToolAllowed(["/B.*/"], "Bash")).toBe(false);
    expect(isToolAllowed(["Bash(npm *)"], "Bash")).toBe(false);
  });

  it("treats '\\\\' as an escape so '\\\\*' matches a literal star", () => {
    expect(isToolAllowed(["\\*literal"], "*literal")).toBe(true);
    expect(isToolAllowed(["\\*literal"], "Xliteral")).toBe(false);
    expect(isToolAllowed(["\\!Bash"], "!Bash")).toBe(true);
  });

  it("does not throw on dangling escape or unusual chars", () => {
    expect(() => isToolAllowed(["\\"], "\\")).not.toThrow();
    expect(() => isToolAllowed(["["], "[")).not.toThrow();
    expect(isToolAllowed(["["], "[")).toBe(true);
    expect(isToolAllowed(["["], "Bash")).toBe(false);
  });
});

describe("isExplicitAll / isLegacyCatalogValue", () => {
  it("recognises the explicit-all sentinel", () => {
    expect(isExplicitAll(["*"])).toBe(true);
    expect(isExplicitAll([])).toBe(false);
    expect(isExplicitAll(["a"])).toBe(false);
    expect(isExplicitAll(null)).toBe(false);
    expect(isExplicitAll(undefined)).toBe(false);
  });

  it("flags only null/undefined as legacy catalog values", () => {
    expect(isLegacyCatalogValue(null)).toBe(true);
    expect(isLegacyCatalogValue(undefined)).toBe(true);
    expect(isLegacyCatalogValue(["*"])).toBe(false);
    expect(isLegacyCatalogValue([])).toBe(false);
  });
});

describe("nextAllowedTools", () => {
  const allToolIds = ["search", "write", "read"];

  it("starts from all tools when removing one from the unrestricted state", () => {
    expect(nextAllowedTools(undefined, allToolIds, "write", false)).toEqual(["search", "read"]);
  });

  it("expands ['*'] before toggling a tool off", () => {
    expect(nextAllowedTools(["*"], allToolIds, "write", false)).toEqual(["search", "read"]);
  });

  it("can re-add a tool after the user removed all selections", () => {
    expect(nextAllowedTools([], allToolIds, "read", true)).toEqual(["read"]);
  });

  it("collapses to ['*'] for the include variant when every tool ends up selected", () => {
    expect(nextAllowedTools(["search", "write"], ["search", "write"], "search", true)).toEqual([
      "*",
    ]);
  });

  it("does NOT collapse to ['*'] for the exclude variant", () => {
    expect(
      nextAllowedTools(["search", "write"], ["search", "write"], "search", true, "exclude"),
    ).toEqual(expect.arrayContaining(["search", "write"]));
  });

  it("starts from no tools when checking one from legacy exclude null", () => {
    expect(nextAllowedTools(null, ["Bash", "Read"], "Bash", true, "exclude")).toEqual(["Bash"]);
  });

  it("keeps legacy exclude null empty when unchecking a tool", () => {
    expect(nextAllowedTools(null, ["Bash", "Read"], "Bash", false, "exclude")).toEqual([]);
  });

  it("preserves the exclude-all wildcard when toggling a tool", () => {
    expect(nextAllowedTools(["*"], ["Bash", "Read"], "Bash", false, "exclude")).toEqual(["*"]);
    expect(nextAllowedTools(["*"], ["Bash", "Read"], "Bash", true, "exclude")).toEqual(["*"]);
  });

  it("preserves glob patterns when unchecking a glob-matched tool", () => {
    expect(
      nextAllowedTools(
        ["mcp__github__*"],
        ["mcp__github__issue", "mcp__github__pr"],
        "mcp__github__issue",
        false,
      ),
    ).toEqual(["mcp__github__*"]);
  });

  it("preserves glob patterns when checking a non-matching tool", () => {
    expect(
      nextAllowedTools(
        ["mcp__github__*"],
        ["mcp__github__issue", "some_other_tool"],
        "some_other_tool",
        true,
      ),
    ).toEqual(["mcp__github__*", "some_other_tool"]);
  });

  it("does not collapse pattern-backed selections to ['*']", () => {
    expect(
      nextAllowedTools(
        ["mcp__github__*"],
        ["mcp__github__issue", "some_other_tool"],
        "some_other_tool",
        true,
      ),
    ).not.toEqual(["*"]);
  });
});

describe("isToolSelectionPatternBacked", () => {
  it("detects tool selections created by a wildcard entry", () => {
    expect(isToolSelectionPatternBacked(["mcp__github__*"], "mcp__github__issue")).toBe(true);
    expect(isToolSelectionPatternBacked(["mcp__github__*"], "Read")).toBe(false);
    expect(isToolSelectionPatternBacked(["*issue"], "mcp__github__read_issue")).toBe(true);
    expect(isToolSelectionPatternBacked(["mcp:*"], "mcp:weather/forecast")).toBe(true);
    // No-longer-supported glob chars are literal, so they don't match those tool ids.
    expect(isToolSelectionPatternBacked(["mcp__github__read?"], "mcp__github__read1")).toBe(false);
    expect(isToolSelectionPatternBacked(["mcp__[ab]*"], "mcp__a_tool")).toBe(false);
  });

  it("does not treat the include explicit-all sentinel as an unmanaged glob", () => {
    expect(isToolSelectionPatternBacked(["*"], "Read")).toBe(false);
  });

  it("treats the exclude explicit-all wildcard as pattern-backed", () => {
    expect(isToolSelectionPatternBacked(["*"], "Read", "exclude")).toBe(true);
  });
});

describe("toolSelectionMode", () => {
  it("classifies undefined as the 'all' mode (legacy)", () => {
    expect(toolSelectionMode(undefined)).toBe("all");
  });

  it("classifies ['*'] as the 'all' mode for include variant", () => {
    expect(toolSelectionMode(["*"])).toBe("all");
  });

  it("classifies [] as 'custom' for include variant (zero tools allowed)", () => {
    expect(toolSelectionMode([])).toBe("custom");
  });

  it("classifies [] as the 'all' mode for exclude variant (block none)", () => {
    expect(toolSelectionMode([], "exclude")).toBe("all");
  });

  it("classifies ['*'] as 'custom' for exclude variant (block everything)", () => {
    expect(toolSelectionMode(["*"], "exclude")).toBe("custom");
  });

  it("treats an explicit subset as 'custom' regardless of variant", () => {
    expect(toolSelectionMode(["search"])).toBe("custom");
    expect(toolSelectionMode(["search"], "exclude")).toBe("custom");
  });
});

describe("applyToolSelectionMode", () => {
  it("returns ['*'] when switching to 'all' for include variant", () => {
    expect(applyToolSelectionMode(["search"], "all", ["search", "write"])).toEqual(["*"]);
  });

  it("returns [] when switching to 'all' for exclude variant", () => {
    expect(applyToolSelectionMode(["search"], "all", ["search", "write"], "exclude")).toEqual([]);
  });

  it("preserves the existing custom list when re-entering custom mode", () => {
    expect(applyToolSelectionMode(["search"], "custom", ["search", "write"])).toEqual(["search"]);
  });

  it("seeds custom mode with every known tool when prior value is undefined", () => {
    expect(applyToolSelectionMode(undefined, "custom", ["a", "b"])).toEqual(["a", "b"]);
  });

  it("seeds exclude custom mode with no tools when prior value is undefined", () => {
    expect(applyToolSelectionMode(undefined, "custom", ["a", "b"], "exclude")).toEqual([]);
  });

  it("seeds exclude custom mode with no tools when prior value is null", () => {
    expect(applyToolSelectionMode(null, "custom", ["a", "b"], "exclude")).toEqual([]);
  });

  it("seeds custom mode with every known tool when prior value is ['*']", () => {
    expect(applyToolSelectionMode(["*"], "custom", ["a", "b"])).toEqual(["a", "b"]);
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
    expect(weather.tools.map((t) => t.id)).toEqual(["mcp:weather/forecast", "mcp:weather/now"]);
  });

  it("uses explicit source field when present, ignoring id prefix", () => {
    const groups = groupToolsBySource([
      { id: "mcp__weather__forecast", source: { kind: "mcp" as const, id: "weather" } },
      { id: "some-tool", source: { kind: "plugin" as const, id: "reminder" } },
      { id: "Bash", source: { kind: "builtin" as const } },
    ]);
    expect(groups.map((g) => g.source.key)).toEqual(["builtin", "plugin:reminder", "mcp:weather"]);
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

  it("treats ['*'] as everything selected", () => {
    expect(groupSelectionState(["*"], groupIds)).toBe("all");
  });

  it("treats null as no group members selected for exclude variant", () => {
    expect(groupSelectionState(null, groupIds, "exclude")).toBe("none");
  });

  it("returns 'none' when no group member is in the allowed list", () => {
    expect(groupSelectionState(["x"], groupIds)).toBe("none");
  });

  it("returns 'some' when only part of the group is selected", () => {
    expect(groupSelectionState(["a"], groupIds)).toBe("some");
  });

  it("returns 'all' when a glob entry covers every group member", () => {
    expect(groupSelectionState(["*"], ["x", "y", "z"])).toBe("all");
  });
});

describe("setGroupSelection", () => {
  const allToolIds = ["a", "b", "c"];

  it("collapses to ['*'] for include variant when every tool is selected", () => {
    expect(setGroupSelection(["c"], allToolIds, ["a", "b"], true)).toEqual(["*"]);
  });

  it("removes every group tool when deselecting", () => {
    expect(setGroupSelection(undefined, allToolIds, ["a", "b"], false)).toEqual(["c"]);
  });

  it("collapses to ['*'] when the result covers every known tool", () => {
    expect(setGroupSelection([], allToolIds, ["a", "b", "c"], true)).toEqual(["*"]);
  });

  it("does NOT collapse to ['*'] for the exclude variant", () => {
    expect(setGroupSelection(["c"], allToolIds, ["a", "b"], true, "exclude")).toEqual(
      expect.arrayContaining(["a", "b", "c"]),
    );
  });

  it("starts from no tools when selecting a group from legacy exclude null", () => {
    expect(setGroupSelection(null, allToolIds, ["a", "b"], true, "exclude")).toEqual(["a", "b"]);
  });

  it("preserves the exclude-all wildcard when toggling a group", () => {
    expect(setGroupSelection(["*"], allToolIds, ["a", "b"], false, "exclude")).toEqual(["*"]);
    expect(setGroupSelection(["*"], allToolIds, ["a", "b"], true, "exclude")).toEqual(["*"]);
  });

  it("preserves glob patterns when clearing a glob-backed group", () => {
    expect(
      setGroupSelection(
        ["mcp__github__*"],
        ["mcp__github__issue", "mcp__github__pr"],
        ["mcp__github__issue", "mcp__github__pr"],
        false,
      ),
    ).toEqual(["mcp__github__*"]);
  });

  it("does not collapse glob-backed group selection to ['*']", () => {
    expect(
      setGroupSelection(
        ["mcp__github__*"],
        ["mcp__github__issue", "some_other_tool"],
        ["some_other_tool"],
        true,
      ),
    ).toEqual(["mcp__github__*", "some_other_tool"]);
  });

  it("ignores group ids that are not part of the catalog", () => {
    expect(setGroupSelection(["a"], allToolIds, ["a", "z"], false)).toEqual([]);
  });
});

describe("toolSelectionPattern", () => {
  it("flags glob entries that match the tool", () => {
    expect(toolSelectionPattern(["mcp__github__*"], "mcp__github__pr")).toBe("mcp__github__*");
  });

  it("flags escaped-literal entries as pattern-backed", () => {
    // Docs advertise `\!` as a literal leading bang. The raw entry differs
    // from the tool id, so the checkbox must surface this as pattern-backed
    // (and disabled) — otherwise a click would try to remove `!Bash`, miss
    // the actual `\!Bash` entry, and leave the user stuck.
    expect(toolSelectionPattern(["\\!Bash"], "!Bash")).toBe("\\!Bash");
    expect(isToolSelectionPatternBacked(["\\!Bash"], "!Bash")).toBe(true);
  });

  it("returns null for exact literal matches", () => {
    expect(toolSelectionPattern(["Bash"], "Bash")).toBe(null);
    expect(isToolSelectionPatternBacked(["Bash"], "Bash")).toBe(false);
  });

  it("returns ['*'] only as the pattern source for exclude variant", () => {
    expect(toolSelectionPattern(["*"], "Bash", "include")).toBe(null);
    expect(toolSelectionPattern(["*"], "Bash", "exclude")).toBe("*");
  });
});
