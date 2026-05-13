import { describe, expect, it } from "vitest";
import { diffPatchableFields, jsonSemanticallyEqual } from "./spec-helpers";
import { type AgentSpec } from "@/lib/config-api";

describe("agent spec helpers", () => {
  it("compares JSON objects with stable key ordering", () => {
    expect(
      jsonSemanticallyEqual(
        { sections: { beta: 2, alpha: 1 } },
        { sections: { alpha: 1, beta: 2 } },
      ),
    ).toBe(true);
  });

  it("builds patch payloads only from patchable semantic changes", () => {
    const original: AgentSpec = {
      id: "agent-a",
      model_id: "m1",
      system_prompt: "stock",
      sections: { alpha: 1, beta: 2 },
    };
    const current: AgentSpec = {
      ...original,
      updated_at: 123,
      sections: { beta: 2, alpha: 1 },
      system_prompt: "patched",
    };

    expect(diffPatchableFields(current, original)).toEqual({ system_prompt: "patched" });
  });
});
