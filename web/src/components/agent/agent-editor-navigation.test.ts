import { describe, expect, it } from "vitest";
import {
  advancedSectionForPath,
  builderSectionForPath,
  stageForPath,
} from "./agent-editor-navigation";

describe("agent authoring navigation", () => {
  it("routes everyday authoring fields into Build", () => {
    // Cause/effect table: ordinary profile input -> Instructions; tool grant,
    // presentation, pattern, or recovery input -> Tools; Skill/MCP ->
    // Integrations; resource input -> Knowledge. Every effect stays in Build.
    expect(stageForPath("system")).toBe("build");
    expect(builderSectionForPath("tools")).toBe("tools");
    expect(stageForPath("tool_patterns")).toBe("build");
    expect(stageForPath("recovery_policies.read")).toBe("build");
    expect(builderSectionForPath("tool_patterns")).toBe("tools");
    expect(builderSectionForPath("recovery_policies.read")).toBe("tools");
    expect(builderSectionForPath("skills")).toBe("integrations");
    expect(builderSectionForPath("resources")).toBe("knowledge");
  });

  it("routes legacy permission issues to the typed ToolSet migration target", () => {
    // Decision rule L1: legacy plugin_config.permission path -> Build / Tools.
    // Effect: authors reach canonical typed ToolSet controls; no legacy policy
    // document or PermissionEditor is reintroduced.
    expect(stageForPath("plugin_config.permission.rules")).toBe("build");
    expect(builderSectionForPath("plugin_config.permission.rules")).toBe("tools");
  });

  it("routes expert mechanisms into Advanced", () => {
    // Cause/effect table: orchestration inputs -> Advanced / Orchestration;
    // remaining extension inputs -> Advanced / Extensions.
    expect(stageForPath("multiagent")).toBe("advanced");
    expect(stageForPath("delegation_limits.max_depth")).toBe("advanced");
    expect(stageForPath("plugin_config.state_machine.states")).toBe("advanced");
    expect(advancedSectionForPath("multiagent")).toBe("orchestration");
    expect(advancedSectionForPath("delegation_limits.max_total")).toBe("orchestration");
    expect(advancedSectionForPath("compaction")).toBe("extensions");
  });
});
