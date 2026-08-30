import { describe, expect, it } from "vitest";
import type { Environment, Session } from "../lib/api/types";
import { managedMoneyLabel, outcomeTone, sessionConfigDestinations, sessionEnvironmentName, sessionMcpPolicies, sessionRuntime, sessionViewFromSearch } from "./session-detail";

function session(model: string): Session {
  return {
    id: "session",
    type: "session",
    agent: {
      id: "agent",
      type: "agent",
      description: null,
      mcp_servers: [],
      model: { id: model },
      multiagent: null,
      name: "Agent",
      skills: [],
      system: null,
      tools: [],
      version: 1,
    },
    archived_at: null,
    budget: null,
    created_at: "2026-01-01T00:00:00Z",
    environment_id: "env_local",
    updated_at: "2026-01-01T00:00:00Z",
    metadata: {},
    resources: [],
    outcome_evaluations: [],
    status: "idle",
    stats: {},
    title: null,
    usage: {},
    vault_ids: [],
  };
}

describe("sessionViewFromSearch", () => {
  it("routes exact event links to Trace and rejects unknown view names", () => {
    // Decision table: an event always owns Trace; a known view is preserved;
    // an unknown or absent view returns the safe conversation default.
    expect(sessionViewFromSearch("artifacts", null)).toBe("artifacts");
    expect(sessionViewFromSearch("artifacts", "evt-1")).toBe("trace");
    expect(sessionViewFromSearch("unknown", null)).toBe("chat");
    expect(sessionViewFromSearch(null, null)).toBe("chat");
  });
});

describe("sessionRuntime", () => {
  it("derives the executed backend from the immutable Managed model coordinate", () => {
    expect(sessionRuntime(session("acp:codex@openai/gpt-5.6-sol"))).toBe("acp:codex");
    expect(sessionRuntime(session("gpt-5.6-sol;executor=acp:codex"))).toBe("acp:codex");
    expect(sessionRuntime(session("executor=acp:codex"))).toBe("acp:codex");
    expect(sessionRuntime(session("a2a:https://agent.example/a2a"))).toBe("A2A remote");
    expect(sessionRuntime(session("deepseek/deepseek-chat"))).toBe("native");
  });
});

describe("sessionEnvironmentName", () => {
  const environment = {
    id: "env_9e41876c",
    type: "environment",
    name: "Release review sandbox",
    config: { type: "cloud" },
    metadata: {},
    created_at: "2026-01-01T00:00:00Z",
    updated_at: "2026-01-01T00:00:00Z",
  } satisfies Environment;

  it("uses a human name while retaining an id fallback for deleted environments", () => {
    expect(sessionEnvironmentName(environment.id, [environment], "Default")).toBe(environment.name);
    expect(sessionEnvironmentName("env_deleted", [environment], "Default")).toBe("env_deleted");
    expect(sessionEnvironmentName(null, [environment], "Default")).toBe("Default");
  });
});

describe("sessionMcpPolicies", () => {
  it("shows the policy frozen into the Session Agent snapshot", () => {
    const value = session("model");
    value.agent.mcp_servers = [{ type: "url", name: "docs", url: "https://docs.example/mcp" }];
    value.agent.tools = [{
      type: "mcp_toolset",
      mcp_server_name: "docs",
      default_config: { enabled: false, permission_policy: { type: "always_allow" } },
      configs: [{ name: "search", enabled: true, permission_policy: { type: "always_ask" } }],
    }];
    expect(sessionMcpPolicies(value.agent)).toEqual([{
      name: "docs",
      enabled: false,
      permission: "always_allow",
      namedOverrides: 1,
    }]);
  });

  it("fails closed for a legacy Session snapshot without a matching ToolSet", () => {
    const value = session("model");
    value.agent.mcp_servers = [{ type: "url", name: "docs", url: "https://docs.example/mcp" }];
    expect(sessionMcpPolicies(value.agent)).toEqual([{
      name: "docs",
      enabled: true,
      permission: "always_ask",
      namedOverrides: 0,
    }]);
  });
});

describe("sessionConfigDestinations", () => {
  it("links every capability proven by the immutable snapshot to its owning current-draft section", () => {
    const value = session("model");
    value.agent.id = "agent/with space";
    value.agent.tools = [{ type: "custom", name: "lookup", description: "Lookup", input_schema: { type: "object" } }];
    value.agent.mcp_servers = [{ type: "url", name: "docs", url: "https://docs.example/mcp" }];
    value.agent.skills = [{ type: "custom", skill_id: "review", version: "1" }];
    const primaryThreadAgent = { ...value.agent, multiagent: null };
    const reviewerThreadAgent = { ...primaryThreadAgent, id: "reviewer", name: "Reviewer", version: 3 };
    value.agent.multiagent = {
      type: "coordinator",
      agents: [primaryThreadAgent, reviewerThreadAgent],
    };

    expect(sessionConfigDestinations("team space", value.agent)).toEqual([
      { kind: "instructions", href: "/w/team%20space/agents/agent%2Fwith%20space?stage=build&section=instructions" },
      { kind: "tools", href: "/w/team%20space/agents/agent%2Fwith%20space?stage=build&section=tools", count: 1 },
      { kind: "integrations", href: "/w/team%20space/agents/agent%2Fwith%20space?stage=build&section=integrations", count: 1 },
      { kind: "knowledge", href: "/w/team%20space/agents/agent%2Fwith%20space?stage=build&section=knowledge", count: 1 },
      { kind: "orchestration", href: "/w/team%20space/agents/agent%2Fwith%20space?stage=advanced&section=orchestration", count: 2 },
    ]);
  });

  it("does not invent links for capabilities absent from the Session snapshot", () => {
    expect(sessionConfigDestinations("default", session("model").agent)).toEqual([
      { kind: "instructions", href: "/w/default/agents/agent?stage=build&section=instructions" },
    ]);
    expect(sessionConfigDestinations("default", undefined)).toEqual([]);
  });
});

describe("Session outcome and usage presentation", () => {
  it("formats integer minor units exactly without floating-point conversion", () => {
    expect(managedMoneyLabel({ amount: "0", currency: "USD" })).toBe("USD 0.00");
    expect(managedMoneyLabel({ amount: "5", currency: "USD" })).toBe("USD 0.05");
    expect(managedMoneyLabel({ amount: "2500", currency: "USD" })).toBe("USD 25.00");
    expect(managedMoneyLabel({ amount: "-50", currency: "USD" })).toBe("USD -0.50");
    expect(managedMoneyLabel({ amount: "2.5", currency: "USD" })).toBeNull();
  });

  it("keeps success, active revision, terminal failure, and pending states distinct", () => {
    expect(outcomeTone("satisfied")).toBe("ok");
    expect(outcomeTone("evaluating")).toBe("warn");
    expect(outcomeTone("needs_revision")).toBe("warn");
    expect(outcomeTone("max_iterations_reached")).toBe("danger");
    expect(outcomeTone("failed")).toBe("danger");
    expect(outcomeTone("pending")).toBe("neutral");
  });
});
