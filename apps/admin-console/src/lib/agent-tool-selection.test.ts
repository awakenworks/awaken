import { describe, expect, it } from "vitest";

import {
  applyToolSelectionMode,
  catalogEntryInspections,
  escapeCatalogLiteral,
  groupSelectionState,
  groupToolsBySource,
  hasUnescapedCatalogWildcard,
  isExplicitAll,
  isLegacyCatalogValue,
  isToolAllowed,
  isToolSelectionPatternBacked,
  nextAllowedTools,
  removeCatalogEntry,
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

  it("requires escaped literal backslash to select the literal backslash tool id", () => {
    // Raw `foo\bar` is interpreted as `foo` + `\b` (literal `b`) + `ar`, so
    // it matches `foobar`, not the tool id that literally contains a
    // backslash. Selecting the literal `foo\bar` requires `foo\\bar`.
    expect(isToolAllowed(["foo\\bar"], "foo\\bar")).toBe(false);
    expect(isToolAllowed(["foo\\bar"], "foobar")).toBe(true);
    expect(isToolAllowed(["foo\\\\bar"], "foo\\bar")).toBe(true);
  });

  it("does not throw on dangling escape or unusual chars", () => {
    expect(() => isToolAllowed(["\\"], "\\")).not.toThrow();
    expect(() => isToolAllowed(["["], "[")).not.toThrow();
    expect(isToolAllowed(["["], "[")).toBe(true);
    expect(isToolAllowed(["["], "Bash")).toBe(false);
  });
});

describe("escapeCatalogLiteral", () => {
  it("returns plain tool ids untouched", () => {
    expect(escapeCatalogLiteral("Bash")).toBe("Bash");
    expect(escapeCatalogLiteral("mcp__github__issue")).toBe("mcp__github__issue");
    expect(escapeCatalogLiteral("")).toBe("");
  });

  it("escapes unescaped stars to literal stars", () => {
    expect(escapeCatalogLiteral("tool*id")).toBe("tool\\*id");
    expect(escapeCatalogLiteral("*literal")).toBe("\\*literal");
    expect(escapeCatalogLiteral("a*b*c")).toBe("a\\*b\\*c");
  });

  it("escapes backslashes before stars to preserve grammar", () => {
    // "tool\id" must encode as "tool\\id" so the runtime treats it as literal.
    expect(escapeCatalogLiteral("tool\\id")).toBe("tool\\\\id");
    // "a\\*b" — backslash + star — escapes both so the runtime still sees
    // literal `\` followed by literal `*`.
    expect(escapeCatalogLiteral("a\\*b")).toBe("a\\\\\\*b");
  });

  it("produces output that round-trips through toolIdMatch", () => {
    // The escaped form must match exactly its source tool id and nothing else.
    expect(isToolAllowed([escapeCatalogLiteral("tool*id")], "tool*id")).toBe(true);
    expect(isToolAllowed([escapeCatalogLiteral("tool*id")], "toolXid")).toBe(false);
    expect(isToolAllowed([escapeCatalogLiteral("tool\\id")], "tool\\id")).toBe(true);
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

describe("catalogEntryInspections", () => {
  it("lists stale exact entries and wildcard entries", () => {
    expect(
      catalogEntryInspections(["Bash", "OldTool", "mcp:*"], [
        "Bash",
        "mcp:weather/forecast",
        "mcp:db/query",
      ]),
    ).toEqual([
      {
        entry: "OldTool",
        exactToolExists: false,
        matchesCurrentToolOnly: false,
        escapedLiteral: false,
        usesWildcard: false,
        matches: [],
      },
      {
        entry: "mcp:*",
        exactToolExists: false,
        matchesCurrentToolOnly: false,
        escapedLiteral: false,
        usesWildcard: true,
        matches: ["mcp:weather/forecast", "mcp:db/query"],
      },
    ]);
  });

  it("does not hide exact current tool ids that contain an unescaped wildcard", () => {
    expect(catalogEntryInspections(["tool*id"], ["tool*id", "toolXid"])).toEqual([
      {
        entry: "tool*id",
        exactToolExists: true,
        matchesCurrentToolOnly: false,
        escapedLiteral: false,
        usesWildcard: true,
        matches: ["tool*id", "toolXid"],
      },
    ]);
  });

  it("does not hide raw backslash entries that equal a current tool id", () => {
    // Catalog grammar treats `\` as the escape character, so a raw `foo\bar`
    // entry whose text happens to equal a current tool id is NOT a safe
    // literal — at runtime `\b` matches a literal `b`, so the entry
    // authorises `foobar` instead of `foo\bar`. The Admin must surface the
    // entry so the user can fix or remove it.
    expect(
      catalogEntryInspections(["foo\\bar"], ["foo\\bar", "foobar"]),
    ).toEqual([
      expect.objectContaining({
        entry: "foo\\bar",
        exactToolExists: true,
        matches: ["foobar"],
      }),
    ]);
  });

  it("classifies escaped literal stars as current exact matches", () => {
    expect(catalogEntryInspections(["foo\\*bar"], ["foo*bar"])).toEqual([
      {
        entry: "foo\\*bar",
        exactToolExists: false,
        matchesCurrentToolOnly: true,
        escapedLiteral: true,
        usesWildcard: false,
        matches: ["foo*bar"],
      },
    ]);
  });

  it("recognises escaped literal star entries", () => {
    expect(hasUnescapedCatalogWildcard("\\*literal")).toBe(false);
    expect(hasUnescapedCatalogWildcard("*literal")).toBe(true);
  });

  it("removes raw catalog entries without expanding patterns", () => {
    expect(removeCatalogEntry(["Bash", "mcp:*", "Read"], "mcp:*")).toEqual(["Bash", "Read"]);
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

  it("preserves wildcard entries that are text-equal to a current tool id", () => {
    expect(nextAllowedTools(["tool*id"], ["tool*id", "toolXid"], "tool*id", false)).toEqual([
      "tool*id",
    ]);
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

  it("escapes literal tool ids when adding via checkbox", () => {
    // Raw `tool*id` would be interpreted by the runtime as a wildcard.
    // The Admin UI must write the escaped form so the entry only authorises
    // the current literal tool id.
    expect(nextAllowedTools([], ["tool*id", "toolXid"], "tool*id", true)).toEqual([
      "tool\\*id",
    ]);
  });

  it("does not collapse text-equal wildcard entries to ['*']", () => {
    // ["tool*id"] is a wildcard entry whose text happens to equal a current
    // tool id. Adding `Other` must NOT auto-collapse to ["*"] because the
    // wildcard would silently expand to cover every future tool too.
    expect(
      nextAllowedTools(["tool*id"], ["tool*id", "Other"], "Other", true),
    ).not.toEqual(["*"]);
  });

  it("does not collapse raw backslash entries to ['*']", () => {
    // Same hazard as the wildcard case: `\` is the catalog escape character,
    // so a raw entry whose text equals a tool id is still a pattern, not a
    // safe literal — auto-collapsing would silently extend it to every
    // future tool.
    expect(
      nextAllowedTools(["foo\\bar"], ["foo\\bar", "Other"], "Other", true),
    ).not.toEqual(["*"]);
  });

  it("preserves raw backslash entries on uncheck", () => {
    // Pattern-like entries (wildcard OR backslash-escaped) are managed
    // through the catalog entry list, not the checkbox row, so unchecking
    // their text-equal tool id must not silently drop them.
    expect(
      nextAllowedTools(["foo\\bar"], ["foo\\bar"], "foo\\bar", false),
    ).toEqual(["foo\\bar"]);
  });

  it("removes the escaped literal entry when unchecking from ['*']", () => {
    // Expanding from ["*"] seeds the baseline with escaped literal entries
    // for any tool id that contains `*` or `\`. Unchecking the matching tool
    // must drop that seed entry — otherwise the tool stays authorised even
    // though the user just turned it off.
    expect(
      nextAllowedTools(["*"], ["tool*id", "Other"], "tool*id", false),
    ).toEqual(["Other"]);
  });

  it("removes the escaped backslash entry when unchecking from ['*']", () => {
    expect(
      nextAllowedTools(["*"], ["foo\\bar", "Other"], "foo\\bar", false),
    ).toEqual(["Other"]);
  });

  it("removes the escaped literal entry when unchecking from legacy allow-all", () => {
    expect(
      nextAllowedTools(undefined, ["tool*id", "Other"], "tool*id", false),
    ).toEqual(["Other"]);
  });

  it("removes a pre-existing escaped literal entry when unchecking it", () => {
    // The Admin already wrote `tool\*id` (the safe escaped form for the
    // current tool id). A subsequent uncheck must remove it — keeping it
    // would leave the tool authorised after the user turned it off.
    expect(
      nextAllowedTools(["tool\\*id", "Other"], ["tool*id", "Other"], "tool*id", false),
    ).toEqual(["Other"]);
  });

  it("refuses to collapse to ['*'] while an unmanaged entry remains", () => {
    // Every current tool id is now covered, but the catalog still holds an
    // unmanaged "unknown" entry — collapsing would silently extend its
    // authority to every future tool too.
    expect(
      nextAllowedTools(["unknown", "a"], ["a", "b"], "b", true),
    ).toEqual(["unknown", "a", "b"]);
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

  it("treats text-equal wildcard entries as pattern-backed", () => {
    expect(toolSelectionPattern(["tool*id"], "tool*id")).toBe("tool*id");
    expect(isToolSelectionPatternBacked(["tool*id"], "tool*id")).toBe(true);
    expect(isToolSelectionPatternBacked(["tool*id"], "toolXid")).toBe(true);
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

  it("escapes literal tool ids when seeding custom mode from legacy state", () => {
    expect(applyToolSelectionMode(undefined, "custom", ["tool*id"])).toEqual([
      "tool\\*id",
    ]);
  });

  it("escapes literal tool ids when seeding custom mode from ['*']", () => {
    expect(applyToolSelectionMode(["*"], "custom", ["tool*id", "Other"])).toEqual([
      "tool\\*id",
      "Other",
    ]);
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

  it("escapes literal tool ids when adding a group selection", () => {
    expect(setGroupSelection([], ["tool*id", "Other"], ["tool*id"], true)).toEqual([
      "tool\\*id",
    ]);
  });

  it("does not collapse to ['*'] when escaped literals cover every tool id", () => {
    // Even though every tool ends up addressable, the escaped literal entry
    // is not text-equal to the raw tool id, so the safest behaviour is to
    // keep the explicit subset instead of writing ["*"].
    expect(
      setGroupSelection([], ["tool*id", "Other"], ["tool*id", "Other"], true),
    ).toEqual(["tool\\*id", "Other"]);
  });

  it("preserves raw backslash entries when clearing a group", () => {
    // A raw `foo\bar` entry is not actually authorising the literal
    // `foo\bar` tool, so clearing the group must not silently delete it;
    // removal stays in the catalog entry list.
    expect(
      setGroupSelection(["foo\\bar", "Other"], ["foo\\bar", "Other"], ["foo\\bar"], false),
    ).toEqual(["foo\\bar", "Other"]);
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

  it("flags text-equal entries with dangling backslash as pattern-backed", () => {
    // Entry and tool id are the same 2-char string `X\`. The matcher treats
    // the dangling escape as a literal `\`, so the entry happens to match
    // its own text. It is still grammatically a pattern (a plain raw
    // literal must be free of catalog escape syntax), so the UI must
    // manage it through the entry list rather than offer a checkbox toggle.
    expect(toolSelectionPattern(["X\\"], "X\\")).toBe("X\\");
    expect(isToolSelectionPatternBacked(["X\\"], "X\\")).toBe(true);
  });
});
