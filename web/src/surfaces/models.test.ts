import { describe, expect, it } from "vitest";
import type { ProviderDriverDescriptor } from "../lib/api/types";
import {
  appendFallback,
  moveFallback,
  profileCandidateOf,
  targetKey,
} from "./model-profile-editor";
import { cloudModelUiState } from "./model-cloud-capability";
import {
  providerConfigurationDefaults,
  providerDraftDefaults,
  providerEndpointCoordinates,
} from "./provider-connection-panel";

// UI cause/effect rules kept beside the executable tests:
// T1: C1 descriptor has a preferred endpoint -> E1 form uses its protocol, URL,
// and stable endpoint id. T2: C2 descriptor has no fixed endpoint -> E2 form
// stays editable with a deterministic placeholder id; it never invents a URL.
// Profile decision table:
// T3 C3 unique fallback -> E3 append after existing order.
// T4 C4 fallback duplicates primary/chain OR chain has 8 -> E4 no mutation.
// T5 C5 move is in bounds -> E5 swap adjacent; out of bounds -> same reference.
// T6 C6 explicit access is BYOK/Cloud/none -> E6 exact/brokered/none binding;
// a BYOK step without a selected credential is rejected before the request.
// Cloud capability decision table:
// T7 unknown capabilities -> E7 no cloud action while loading.
// T8 cloud models off (regardless of Cloud login) -> E8 local/BYOK-only UI.
// T9 cloud models on + unauthenticated/authenticated -> E9 sign-in-required/ready.

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

  it("derives Vertex endpoint coordinates from descriptor-owned configuration fields", () => {
    const vertex: ProviderDriverDescriptor = {
      ...openai,
      provider_kind: "vertex",
      auth_methods: ["oauth"],
      supported_dialects: ["vertex_gemini"],
      configuration_fields: [
        { key: "project_id", label: "Project", kind: "text", required: true, advanced: false },
        { key: "location", label: "Location", kind: "text", required: true, advanced: false, placeholder: "global" },
      ],
      default_endpoints: [],
    };
    expect(providerConfigurationDefaults(vertex)).toEqual({
      project_id: "",
      location: "global",
    });
    expect(
      providerEndpointCoordinates(
        vertex,
        { endpoint: "vertex-gemini", baseUrl: "" },
        { project_id: "demo-project", location: "us-central1" },
      ),
    ).toEqual({
      endpoint: "vertex-gemini",
      baseUrl:
        "https://us-central1-aiplatform.googleapis.com/v1/projects/demo-project/locations/us-central1/",
    });
  });
});

describe("cloud model capability state", () => {
  const capabilities = (cloudModels: boolean, authenticated: boolean) => ({
    identity: {
      mode: "awaken-cloud" as const,
      cloud_login_enabled: true,
      authenticated,
    },
    models: {
      local_catalog_enabled: true,
      byok_enabled: true,
      cloud_models_enabled: cloudModels,
    },
  });

  it("fails closed while capabilities are unknown or Cloud models are disabled", () => {
    expect(cloudModelUiState(undefined)).toBe("loading");
    expect(cloudModelUiState(capabilities(false, false))).toBe("local");
    expect(cloudModelUiState(capabilities(false, true))).toBe("local");
  });

  it("requires both the Cloud-model switch and an authenticated session", () => {
    expect(cloudModelUiState(capabilities(true, false))).toBe("sign_in_required");
    expect(cloudModelUiState(capabilities(true, true))).toBe("ready");
  });
});

describe("inference profile draft", () => {
  const primary = targetKey({ model_id: "m1", provider_id: "p1", protocol_endpoint_id: "e1" });
  const fallback = targetKey({ model_id: "m2", provider_id: "p2", protocol_endpoint_id: "e2" });

  it("appends only unique explicit fallbacks and enforces the eight-step limit", () => {
    const one = appendFallback([], fallback, primary);
    expect(one).toEqual([{ targetKey: fallback, accessMode: "none", credentialId: "" }]);
    expect(appendFallback(one, fallback, primary)).toBe(one);
    expect(appendFallback(one, primary, primary)).toBe(one);
    const full = Array.from({ length: 8 }, (_, index) => ({
      targetKey: `target-${index}`,
      accessMode: "none" as const,
      credentialId: "",
    }));
    expect(appendFallback(full, "target-9", primary)).toBe(full);
  });

  it("reorders only adjacent in-range fallbacks", () => {
    const items = [
      { targetKey: "a", accessMode: "none" as const, credentialId: "" },
      { targetKey: "b", accessMode: "exact" as const, credentialId: "cred" },
    ];
    expect(moveFallback(items, 1, -1).map((item) => item.targetKey)).toEqual(["b", "a"]);
    expect(moveFallback(items, 0, -1)).toBe(items);
    expect(moveFallback(items, 1, 1)).toBe(items);
  });

  it("keeps each model target paired with its own credential binding", () => {
    expect(profileCandidateOf({ targetKey: primary, accessMode: "exact", credentialId: "cred-1" })).toEqual({
      target: { model_id: "m1", provider_id: "p1", protocol_endpoint_id: "e1" },
      credential_binding: { type: "exact", credential_source_id: "cred-1" },
    });
    expect(profileCandidateOf({ targetKey: fallback, accessMode: "none", credentialId: "" }).credential_binding).toEqual({
      type: "none",
    });
    expect(profileCandidateOf({ targetKey: fallback, accessMode: "brokered", credentialId: "" }).credential_binding).toEqual({
      type: "brokered",
    });
    expect(() => profileCandidateOf({ targetKey: fallback, accessMode: "exact", credentialId: "" })).toThrow(
      "Choose a BYOK credential",
    );
  });
});
