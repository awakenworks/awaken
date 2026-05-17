import { describe, expect, it } from "vitest";

import {
  deriveAllowedMode,
  isToolAllowed,
  patternMatches,
  patternMatchesIn,
} from "./tool-catalog";

describe("patternMatches", () => {
  it("matches literals exactly", () => {
    expect(patternMatches("Bash", "Bash")).toBe(true);
    expect(patternMatches("Bash", "bash")).toBe(false);
    expect(patternMatches("Bash", "Bashx")).toBe(false);
  });

  it("* is universal", () => {
    expect(patternMatches("*", "")).toBe(true);
    expect(patternMatches("*", "mcp:fs/read")).toBe(true);
  });

  it("* in prefix / suffix / middle", () => {
    expect(patternMatches("mcp:*", "mcp:weather")).toBe(true);
    expect(patternMatches("mcp:*", "Bash")).toBe(false);
    expect(patternMatches("*Tool", "BashTool")).toBe(true);
    expect(patternMatches("mcp:*/read", "mcp:fs/read")).toBe(true);
    expect(patternMatches("mcp:*/read", "mcp:fs/write")).toBe(false);
  });

  it("escape preserves literal *", () => {
    expect(patternMatches("foo\\*bar", "foo*bar")).toBe(true);
    expect(patternMatches("foo\\*bar", "fooXbar")).toBe(false);
  });

  it("escape preserves literal backslash", () => {
    expect(patternMatches("foo\\\\bar", "foo\\bar")).toBe(true);
    expect(patternMatches("foo\\\\bar", "foobar")).toBe(false);
  });
});

describe("isToolAllowed", () => {
  it("default (everything undefined) blocks all", () => {
    expect(isToolAllowed({}, "Bash")).toBe(false);
  });

  it("literal allow alone permits exact match", () => {
    expect(isToolAllowed({ allowed_tools: ["Bash"] }, "Bash")).toBe(true);
    expect(isToolAllowed({ allowed_tools: ["Bash"] }, "Write")).toBe(false);
  });

  it("pattern allow alone permits matches", () => {
    expect(isToolAllowed({ allowed_tool_patterns: ["mcp:*"] }, "mcp:weather")).toBe(true);
    expect(isToolAllowed({ allowed_tool_patterns: ["mcp:*"] }, "Bash")).toBe(false);
  });

  it("literal exclude overrides pattern allow", () => {
    expect(
      isToolAllowed(
        {
          allowed_tool_patterns: ["*"],
          excluded_tools: ["Bash"],
        },
        "Bash",
      ),
    ).toBe(false);
  });

  it("pattern exclude overrides pattern allow", () => {
    expect(
      isToolAllowed(
        {
          allowed_tool_patterns: ["*"],
          excluded_tool_patterns: ["dangerous-*"],
        },
        "dangerous-delete",
      ),
    ).toBe(false);
  });

  it("literal + pattern union", () => {
    const catalog = {
      allowed_tools: ["Bash"],
      allowed_tool_patterns: ["mcp:*"],
    };
    expect(isToolAllowed(catalog, "Bash")).toBe(true);
    expect(isToolAllowed(catalog, "mcp:weather")).toBe(true);
    expect(isToolAllowed(catalog, "Read")).toBe(false);
  });
});

describe("patternMatchesIn", () => {
  it("returns matching subset", () => {
    expect(patternMatchesIn("mcp:*", ["mcp:fs", "Bash", "mcp:weather"])).toEqual([
      "mcp:fs",
      "mcp:weather",
    ]);
  });

  it("returns empty for no matches", () => {
    expect(patternMatchesIn("mcp:*", ["Bash", "Read"])).toEqual([]);
  });
});

describe("deriveAllowedMode", () => {
  // Label-vs-matcher parity: the badge must match what the matcher does.
  // Absence is deny-all per `isToolAllowed`; only the universal `"*"`
  // glob in `allowed_tool_patterns` is "all".

  it("returns 'all' when allowed_tool_patterns contains '*'", () => {
    expect(deriveAllowedMode({ allowed_tool_patterns: ["*"] })).toBe("all");
    expect(deriveAllowedMode({ allowed_tool_patterns: ["mcp:*", "*"] })).toBe("all");
  });

  it("returns 'custom' when both allow fields are absent (deny-all)", () => {
    expect(deriveAllowedMode({ allowed_tool_patterns: undefined })).toBe("custom");
  });

  it("returns 'custom' when allow fields are explicit empty lists (cleared via PATCH)", () => {
    expect(deriveAllowedMode({ allowed_tool_patterns: [] })).toBe("custom");
  });

  it("returns 'custom' when only a non-universal pattern is set", () => {
    expect(deriveAllowedMode({ allowed_tool_patterns: ["mcp:*"] })).toBe("custom");
  });
});
