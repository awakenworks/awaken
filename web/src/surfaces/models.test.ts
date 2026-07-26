import { describe, expect, it } from "vitest";
import type { ProviderDriverDescriptor } from "../lib/api/types";
import {
  appendFallback,
  moveFallback,
  profileCandidateOf,
  targetKey,
} from "./model-profile-editor";
import { providerDraftDefaults } from "./models";

// UI cause/effect rules kept beside the executable tests:
// T1: C1 descriptor has a preferred endpoint -> E1 form uses its protocol, URL,
// and stable endpoint id. T2: C2 descriptor has no fixed endpoint -> E2 form
// stays editable with a deterministic placeholder id; it never invents a URL.
// Profile decision table:
// T3 C3 unique fallback -> E3 append after existing order.
// T4 C4 fallback duplicates primary/chain OR chain has 8 -> E4 no mutation.
// T5 C5 move is in bounds -> E5 swap adjacent; out of bounds -> same reference.
// T6 C6 credential id present/absent -> E6 exact/none binding on that candidate.

const openai: ProviderDriverDescriptor = {
  provider_kind: "openai",
  display_name: "OpenAI",
  supported_dialects: ["open_ai_responses", "open_ai_chat"],
  auth_methods: ["api_key"],
  configuration_fields: [],
  default_endpoints: [
    {
      id_suffix: "responses",
      dialect: "open_ai_responses",
      base_url: "https://api.openai.com/v1",
    },
  ],
  supports_model_discovery: true,
};

describe("providerDraftDefaults", () => {
  it("uses the backend descriptor's preferred protocol and endpoint", () => {
    expect(providerDraftDefaults(openai)).toEqual({
      provider: "openai",
      endpoint: "openai-responses",
      baseUrl: "https://api.openai.com/v1",
      dialect: "open_ai_responses",
      model: "",
    });
  });

  it("keeps providers without a fixed endpoint configurable", () => {
    expect(
      providerDraftDefaults({
        ...openai,
        provider_kind: "vertex",
        supported_dialects: ["vertex_gemini"],
        default_endpoints: [],
      }),
    ).toMatchObject({
      endpoint: "vertex-endpoint",
      baseUrl: "",
      dialect: "vertex_gemini",
    });
  });
});

describe("inference profile draft", () => {
  const primary = targetKey({ model_id: "m1", provider_id: "p1", protocol_endpoint_id: "e1" });
  const fallback = targetKey({ model_id: "m2", provider_id: "p2", protocol_endpoint_id: "e2" });

  it("appends only unique explicit fallbacks and enforces the eight-step limit", () => {
    const one = appendFallback([], fallback, primary);
    expect(one).toEqual([{ targetKey: fallback, credentialId: "" }]);
    expect(appendFallback(one, fallback, primary)).toBe(one);
    expect(appendFallback(one, primary, primary)).toBe(one);
    const full = Array.from({ length: 8 }, (_, index) => ({
      targetKey: `target-${index}`,
      credentialId: "",
    }));
    expect(appendFallback(full, "target-9", primary)).toBe(full);
  });

  it("reorders only adjacent in-range fallbacks", () => {
    const items = [
      { targetKey: "a", credentialId: "" },
      { targetKey: "b", credentialId: "cred" },
    ];
    expect(moveFallback(items, 1, -1).map((item) => item.targetKey)).toEqual(["b", "a"]);
    expect(moveFallback(items, 0, -1)).toBe(items);
    expect(moveFallback(items, 1, 1)).toBe(items);
  });

  it("keeps each model target paired with its own credential binding", () => {
    expect(profileCandidateOf({ targetKey: primary, credentialId: "cred-1" })).toEqual({
      target: { model_id: "m1", provider_id: "p1", protocol_endpoint_id: "e1" },
      credential_binding: { type: "exact", credential_source_id: "cred-1" },
    });
    expect(profileCandidateOf({ targetKey: fallback, credentialId: "" }).credential_binding).toEqual({
      type: "none",
    });
  });
});
