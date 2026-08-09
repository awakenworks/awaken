import { describe, expect, it } from "vitest";
import type { AgentConfigItem } from "./api/types";
import {
  agentTarget,
  delegateTargetView,
  projectCollaborations,
  withRoster,
} from "./agent-collaboration";

const agent = (id: string, multiagent?: AgentConfigItem["multiagent"], metadata: Record<string, string> = {}): AgentConfigItem => ({
  id,
  model: { mode: "auto" },
  tools: [],
  mcp_servers: [],
  skills: [],
  max_steps: 8,
  plugins: [],
  plugin_config: {},
  context_policy: { kind: "keep_all" },
  multiagent,
  metadata,
});

describe("Agent collaboration projection", () => {
  it("normalizes dynamic, pinned and self roster entries", () => {
    expect(delegateTargetView("researcher", "lead")).toEqual({ id: "researcher", recursiveSelf: false });
    expect(delegateTargetView({ type: "agent", id: "coder", version: 4 }, "lead"))
      .toEqual({ id: "coder", version: 4, recursiveSelf: false });
    expect(delegateTargetView({ type: "self" }, "lead"))
      .toEqual({ id: "lead", recursiveSelf: true });
    expect(withRoster([agentTarget("coder", 4)])).toEqual({
      type: "coordinator",
      agents: [{ type: "agent", id: "coder", version: 4 }],
    });
  });

  it("derives coordinators, dependencies, broken references and unused Agents", () => {
    const projected = projectCollaborations([
      agent("lead", { type: "coordinator", agents: ["researcher", "missing", { type: "self" }] }),
      agent("researcher", undefined, { "awaken.parent_agent_id": "lead", "awaken.agent_role": "auxiliary" }),
      agent("unused", undefined, { "awaken.parent_agent_id": "lead", "awaken.agent_role": "auxiliary" }),
    ]);
    expect(projected.coordinators.map((item) => item.id)).toEqual(["lead"]);
    expect([...projected.referencedAgentIds]).toEqual(["researcher", "missing"]);
    expect(projected.brokenReferences).toEqual([{ coordinatorId: "lead", targetId: "missing" }]);
    expect([...projected.recursiveCoordinatorIds]).toEqual(["lead"]);
    expect(projected.unusedAgents.map((item) => item.id)).toEqual(["unused"]);
  });
});
