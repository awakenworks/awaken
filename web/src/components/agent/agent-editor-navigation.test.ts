import { describe, expect, it } from "vitest";
import {
  advancedSectionForPath,
  builderSectionForPath,
  stageForPath,
} from "./agent-editor-navigation";

describe("agent authoring navigation", () => {
  it("routes everyday authoring fields into Build", () => {
    expect(stageForPath("system")).toBe("build");
    expect(builderSectionForPath("tools")).toBe("tools");
    expect(builderSectionForPath("plugin_config.permission.rules")).toBe("tools");
    expect(builderSectionForPath("skills")).toBe("integrations");
    expect(builderSectionForPath("resources")).toBe("knowledge");
  });

  it("routes expert mechanisms into Advanced", () => {
    expect(stageForPath("multiagent")).toBe("advanced");
    expect(stageForPath("plugin_config.state_machine.states")).toBe("advanced");
    expect(advancedSectionForPath("multiagent")).toBe("orchestration");
    expect(advancedSectionForPath("compaction")).toBe("extensions");
  });
});
