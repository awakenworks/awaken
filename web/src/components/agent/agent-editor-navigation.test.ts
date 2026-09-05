import { describe, expect, it } from "vitest";
import {
  advancedSectionForPath,
  stageForQuery,
  builderSectionForQuery,
  advancedSectionForQuery,
  builderSectionForPath,
  stageForPath,
} from "./agent-editor-navigation";

describe("agent authoring navigation", () => {
  it("uses one total query decision table for initial render and route reset", () => {
    // Causes: stage and section independently absent, invalid, or recognized.
    // Constraints: sections belong to one family; unrelated query keys are inert.
    // Effects: recognized values survive exactly; each invalid/absent value uses
    // its family default. Cartesian rules N1-N50 cover both valid and fallback
    // branches, including a section belonging to the inactive stage.
    const stages = [[null, "quickstart"], ["bad", "quickstart"], ["quickstart", "quickstart"], ["build", "build"], ["advanced", "advanced"]];
    const sections = [
      [null, "instructions", "orchestration"], ["bad", "instructions", "orchestration"],
      ["instructions", "instructions", "orchestration"], ["tools", "tools", "orchestration"],
      ["integrations", "integrations", "orchestration"], ["knowledge", "knowledge", "orchestration"],
      ["orchestration", "instructions", "orchestration"], ["extensions", "instructions", "extensions"],
      ["source", "instructions", "source"], ["release", "instructions", "release"],
    ];
    for (const [inputStage, stage] of stages) for (const [section, builderSection, advancedSection] of sections) {
      const query = new URLSearchParams("parent=unchanged");
      if (inputStage !== null) query.set("stage", inputStage);
      if (section !== null) query.set("section", section);
      const before = query.toString();
      expect({
        stage: stageForQuery(query.get("stage")),
        builderSection: builderSectionForQuery(query.get("section")),
        advancedSection: advancedSectionForQuery(query.get("section")),
      }).toEqual({ stage, builderSection, advancedSection });
      expect(query.toString()).toBe(before);
    }
  });

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
