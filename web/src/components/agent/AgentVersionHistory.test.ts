import { describe, expect, it } from "vitest";
import type { Agent } from "../../lib/api/types";
import { agentVersionModel, orderedAgentVersions } from "./AgentVersionHistory";

function version(version: number, model: Agent["model"] = { id: "provider/model" }): Agent {
  return {
    id: "agent_release",
    type: "agent",
    name: "Release reviewer",
    model,
    tools: [],
    mcp_servers: [],
    skills: [],
    metadata: {},
    version,
    status: "published",
    created_at: "2026-08-29T00:00:00Z",
    updated_at: `2026-08-29T00:00:0${version}Z`,
  };
}

describe("Agent version history presentation", () => {
  it("orders immutable releases newest-first without changing the API page", () => {
    const source = [version(1), version(3), version(2)];
    expect(orderedAgentVersions(source).map((item) => item.version)).toEqual([3, 2, 1]);
    expect(source.map((item) => item.version)).toEqual([1, 3, 2]);
  });

  it("renders both official model coordinate representations", () => {
    expect(agentVersionModel(version(1, "provider/model"))).toBe("provider/model");
    expect(agentVersionModel(version(1, { id: "provider/model", speed: "fast" }))).toBe("provider/model");
  });
});
