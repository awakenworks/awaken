import { describe, expect, it } from "vitest";
import {
  agentModelIsRunnable,
  agentNeedsToolBridge,
  BLANK_AGENT_CONFIG,
  buildAgentDraftBody,
} from "./AgentEditorChrome";
import { starterAgentId } from "./AgentQuickstart";
import type { RuntimeCap } from "../../lib/api/types";

describe("starterAgentId", () => {
  it("fills only a blank draft id and preserves an authored identity", () => {
    // Cause/effect decision table: R1 blank/whitespace draft id + Starter ->
    // use the Starter's stable id; R2 authored draft id + Starter -> preserve
    // the exact authored id. A template removes one setup chore without
    // becoming a second identity owner after the operator has made a choice.
    expect(starterAgentId("", "repository-change")).toBe("repository-change");
    expect(starterAgentId("   ", "repository-change")).toBe("repository-change");
    expect(starterAgentId("release-reviewer", "repository-change")).toBe("release-reviewer");
  });
});

describe("agent editor draft derivation", () => {
  it("requires the governed bridge exactly when the draft selects bridge-owned capabilities", () => {
    // Cause/effect decision table: R1 no tool/skill/MCP/memory configuration ->
    // direct model execution needs no bridge; R2 any one of those four sources
    // -> require a runtime that advertises the Awaken tool bridge. Testing each
    // source independently prevents an authoring surface from silently making
    // one capability runnable through a weaker execution path.
    const blank = { ...BLANK_AGENT_CONFIG };
    expect(agentNeedsToolBridge(blank), "R1").toBe(false);
    expect(agentNeedsToolBridge({
      ...blank,
      tools: [{ type: "custom", name: "review", description: "test", input_schema: {} }],
    }), "R2 tool").toBe(true);
    expect(agentNeedsToolBridge({ ...blank, skills: ["review"] }), "R2 skill").toBe(true);
    expect(agentNeedsToolBridge({
      ...blank,
      mcp_servers: [{ name: "repo", url: "https://mcp.example.test" }],
    }), "R2 MCP").toBe(true);
    expect(agentNeedsToolBridge({ ...blank, plugin_config: { memory: {} } }), "R2 memory")
      .toBe(true);
    expect(agentNeedsToolBridge({ ...blank, tools: ["web_search"] }, ["web_search"]), "R3 provider server")
      .toBe(false);
  });

  it("adds the transient permission preset only to the requested authoring body", () => {
    // Cause/effect decision table: R1 no preset -> replace the route-owned id
    // without adding permission_preset; R2 controlled preset -> add that exact
    // transient request field. Neither rule mutates the persisted draft input.
    const draft = { ...BLANK_AGENT_CONFIG, id: "draft-id" };
    expect(buildAgentDraftBody(draft, "route-id", null)).toEqual({
      ...draft,
      id: "route-id",
    });
    expect(buildAgentDraftBody(draft, "route-id", "controlled_modifications"))
      .toEqual({ ...draft, id: "route-id", permission_preset: "controlled_modifications" });
    expect(draft).not.toHaveProperty("permission_preset");
  });

  it("derives runnable state from the selected model authority", () => {
    // Cause/effect decision table: R1 string/id + ready model -> runnable;
    // R2 string/id + absent model -> blocked; R3 backend + matching runtime
    // not explicitly undetected -> runnable; R4 absent/undetected backend ->
    // blocked; R5 automatic selection -> runnable iff any model is ready.
    const supportedFeatures: NonNullable<RuntimeCap["features"]> = {
      environment_session: "supported",
      context_projection: "supported",
      awaken_tool_bridge: "supported",
      state_machine: "unavailable",
      background_tools: "unavailable",
      working_directory: "supported",
      provider_server_tools: "conditional",
    };
    const runtimes: RuntimeCap[] = [{
      id: "acp:ready",
      label: "Ready ACP",
      kind: "acp" as const,
      description: "test",
      local: { detected: true, login_state: "available" },
      features: supportedFeatures,
    }, {
      id: "acp:undetected",
      label: "Undetected ACP",
      kind: "acp" as const,
      description: "test",
      local: { detected: false },
    }];
    expect(agentModelIsRunnable("ready-model", ["ready-model"], runtimes), "R1 string").toBe(true);
    expect(agentModelIsRunnable({ id: "ready-model" }, ["ready-model"], runtimes), "R1 id").toBe(true);
    expect(agentModelIsRunnable("missing-model", ["ready-model"], runtimes), "R2").toBe(false);
    expect(agentModelIsRunnable({ mode: "backend_default", backend_ref: "acp:ready" }, [], runtimes), "R3").toBe(true);
    expect(agentModelIsRunnable({
      mode: "backend_default",
      backend_ref: "acp:ready",
      configuration: { working_directory: "../outside" },
    }, [], runtimes), "R3 invalid cwd").toBe(false);
    expect(agentModelIsRunnable({ mode: "backend_default", backend_ref: "acp:ready" }, [], [{
      ...runtimes[0],
      features: { ...supportedFeatures, awaken_tool_bridge: "conditional" },
    }], true), "R3 unverified tool bridge").toBe(false);
    expect(agentModelIsRunnable({ mode: "backend_exact", backend_ref: "acp:undetected", model_ref: "m" }, [], runtimes), "R4").toBe(false);
    expect(agentModelIsRunnable({ mode: "backend_default", backend_ref: "acp:missing" }, [], runtimes), "R4 absent").toBe(false);
    expect(agentModelIsRunnable({ mode: "auto" }, ["ready-model"], runtimes), "R5 ready").toBe(true);
    expect(agentModelIsRunnable({ mode: "auto" }, [], runtimes), "R5 absent").toBe(false);
  });
});
