import { describe, expect, it } from "vitest";
import { A2A_SERVER_ID_OPTION, a2aServerIdForAgent, isDiscoveredA2aAgent } from "./a2a-agent";
import type { AgentSpec } from "./config-api";

function agent(patch: Partial<AgentSpec>): AgentSpec {
  return {
    id: "agent-a",
    model_id: "model-a",
    system_prompt: "",
    max_rounds: 8,
    plugin_ids: [],
    delegates: [],
    sections: {},
    ...patch,
  };
}

describe("A2A agent provenance helpers", () => {
  it("does not classify non-A2A registry agents as discovered A2A agents", () => {
    const spec = agent({ registry: "legacy-registry" });

    expect(a2aServerIdForAgent(spec)).toBeNull();
    expect(isDiscoveredA2aAgent(spec)).toBe(false);
  });

  it("requires explicit A2A server provenance on A2A endpoints", () => {
    const spec = agent({
      registry: "legacy-registry",
      endpoint: { backend: "a2a", base_url: "https://remote.example.com/a2a" },
    });

    expect(a2aServerIdForAgent(spec)).toBeNull();
    expect(isDiscoveredA2aAgent(spec)).toBe(false);
  });

  it("classifies A2A endpoints with an a2a_server_id option as discovered", () => {
    const spec = agent({
      endpoint: {
        backend: "a2a",
        base_url: "https://remote.example.com/a2a",
        options: { [A2A_SERVER_ID_OPTION]: "partner" },
      },
    });

    expect(a2aServerIdForAgent(spec)).toBe("partner");
    expect(isDiscoveredA2aAgent(spec)).toBe(true);
  });
});
