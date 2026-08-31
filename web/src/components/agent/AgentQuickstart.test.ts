import { describe, expect, it } from "vitest";
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
