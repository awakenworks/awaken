import { describe, expect, it } from "vitest";
import {
  agentModelIsRunnable,
  BLANK_AGENT_CONFIG,
  buildAgentDraftBody,
} from "./AgentEditorChrome";
import { starterAgentId } from "./AgentQuickstart";

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
    const runtimes = [{
      id: "acp:ready",
      label: "Ready ACP",
      kind: "acp" as const,
      description: "test",
      local: { detected: true },
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
    expect(agentModelIsRunnable({ mode: "backend_exact", backend_ref: "acp:undetected", model_ref: "m" }, [], runtimes), "R4").toBe(false);
    expect(agentModelIsRunnable({ mode: "backend_default", backend_ref: "acp:missing" }, [], runtimes), "R4 absent").toBe(false);
    expect(agentModelIsRunnable({ mode: "auto" }, ["ready-model"], runtimes), "R5 ready").toBe(true);
    expect(agentModelIsRunnable({ mode: "auto" }, [], runtimes), "R5 absent").toBe(false);
  });
});
