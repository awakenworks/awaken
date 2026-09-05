import { describe, expect, it } from "vitest";
import {
  acpModelChoices,
  defaultSelectionForRuntime,
  executionRuntimeId,
  isAcpModelSelection,
} from "./agent-model-selection";
import type { RuntimeCap } from "./api/types";

const runtime = (login_state: string, choices: string[]): RuntimeCap => ({
  id: "acp:codex",
  label: "Codex",
  kind: "acp",
  cli: "codex",
  description: "test",
  local: {
    detected: true,
    login_state,
    negotiated: {
      modes: [],
      config_options: [{
        native_id: "model",
        name: "Model",
        current_value: "default",
        choices: choices.map((native_value) => ({ native_value, name: native_value })),
      }],
    },
  },
});

describe("ACP model choices", () => {
  it("derives only executable default/exact selections from live capabilities", () => {
    // Cause/effect decision table:
    // U1 login available + model choices -> one default plus every exact choice.
    // U2 login unavailable             -> no selectable backend.
    // U3 available but no model option -> default only.
    expect(acpModelChoices([runtime("available", ["gpt-a", "gpt-b"])]).map((row) => row.selection))
      .toEqual([
        { mode: "backend_default", backend_ref: "acp:codex" },
        { mode: "backend_exact", backend_ref: "acp:codex", model_ref: "gpt-a" },
        { mode: "backend_exact", backend_ref: "acp:codex", model_ref: "gpt-b" },
      ]);
    expect(acpModelChoices([runtime("login_required", ["gpt-a"])]), "U2").toEqual([]);
    expect(acpModelChoices([runtime("available", [])]), "U3").toHaveLength(1);
  });

  it("derives one execution root and resets ACP-specific configuration on a runtime change", () => {
    const configured = {
      mode: "backend_exact" as const,
      backend_ref: "acp:codex",
      model_ref: "gpt-a",
      configuration: { mode: "plan", options: { reasoning_effort: "high" } },
    };
    expect(isAcpModelSelection(configured)).toBe(true);
    expect(executionRuntimeId(configured)).toBe("acp:codex");
    expect(executionRuntimeId({ mode: "auto" })).toBe("awaken");
    expect(defaultSelectionForRuntime(runtime("available", ["gpt-a"]))).toEqual({
      mode: "backend_default",
      backend_ref: "acp:codex",
    });
    expect(defaultSelectionForRuntime(runtime("login_required", ["gpt-a"]))).toBeNull();
  });
});

// R1 BackendOwned, R2 Provider target, R3 legacy pinned ACP must use the same
// capability gating; Native targets remain Native. Provisioning is not runtime.
it("routes provider ACP authoring through the reported adapter capabilities", () => {
  for (const model of [
    { mode: "target", backend_ref: "acp:claude", target: { model_id: "test" } },
    { mode: "pinned", backend_ref: "acp:claude", model_ref: "test", provider_identity_ref: "p" },
  ] as const) {
    expect(isAcpModelSelection(model)).toBe(true);
    expect(executionRuntimeId(model)).toBe("acp:claude");
  }
  expect(isAcpModelSelection({ mode: "target", backend_ref: "genai", target: { model_id: "test" } })).toBe(false);
});
