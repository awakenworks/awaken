import { describe, expect, it } from "vitest";
import { diffConfig, labelForPath, sectionForPath } from "./config-diff";

describe("diffConfig", () => {
  it("returns no changes for equal configs", () => {
    expect(diffConfig({ a: 1, b: [1, 2] }, { a: 1, b: [1, 2] })).toEqual([]);
  });

  it("reports a scalar change with its dotted path", () => {
    expect(diffConfig({ max_steps: 8 }, { max_steps: 12 })).toEqual([
      { path: "max_steps", kind: "changed", before: 8, after: 12 },
    ]);
  });

  it("recurses into nested objects", () => {
    const before = { plugin_config: { compact: { threshold: 40, keep_last: 8 } } };
    const after = { plugin_config: { compact: { threshold: 60, keep_last: 8 } } };
    expect(diffConfig(before, after)).toEqual([
      { path: "plugin_config.compact.threshold", kind: "changed", before: 40, after: 60 },
    ]);
  });

  it("reports added and removed keys", () => {
    const changes = diffConfig({ a: 1 }, { b: 2 });
    expect(changes).toContainEqual({ path: "a", kind: "removed", before: 1 });
    expect(changes).toContainEqual({ path: "b", kind: "added", after: 2 });
  });

  it("treats an array as a single leaf change", () => {
    expect(diffConfig({ tools: ["read"] }, { tools: ["read", "write"] })).toEqual([
      { path: "tools", kind: "changed", before: ["read"], after: ["read", "write"] },
    ]);
  });
});

describe("labelForPath", () => {
  it("maps a known top-level path to its domain term", () => {
    expect(labelForPath("system")).toBe("System instructions");
    expect(labelForPath("plugins")).toBe("Enabled behaviors");
  });
  it("falls back to the top segment's label for a nested path", () => {
    expect(labelForPath("plugin_config.compact.threshold")).toBe("Behavior config");
  });
  it("falls back to the raw path when unknown (nothing hidden)", () => {
    expect(labelForPath("mystery.field")).toBe("mystery.field");
  });
});

describe("sectionForPath", () => {
  it("routes tool paths to Tools", () => {
    expect(sectionForPath("tools")).toBe("tools");
    expect(sectionForPath("tool_overrides")).toBe("tools");
  });
  it("routes plugin/context paths to Behavior", () => {
    expect(sectionForPath("plugin_config.compact.threshold")).toBe("behavior");
    expect(sectionForPath("context_policy")).toBe("behavior");
  });
  it("routes managed Agent integrations to Integrations", () => {
    for (const path of ["mcp_servers", "skills", "multiagent", "metadata.owner"]) {
      expect(sectionForPath(path)).toBe("integrations");
    }
  });
  it("routes model/system/whole-config to Overview", () => {
    expect(sectionForPath("model")).toBe("overview");
    expect(sectionForPath("system")).toBe("overview");
    expect(sectionForPath("")).toBe("overview");
  });
});
