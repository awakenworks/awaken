import { describe, expect, it } from "vitest";
import type { ProviderDriverDescriptor } from "../lib/api/types";
import { cloudModelUiState } from "./model-cloud-capability";
import {
  providerConfigurationDefaults,
  providerDraftDefaults,
} from "./provider-connection-panel";

// UI cause/effect rules kept beside the executable tests:
// T1: C1 descriptor has a preferred endpoint -> E1 form uses its protocol, URL,
// and stable endpoint id. T2: C2 descriptor has no fixed endpoint -> E2 form
// stays editable with a deterministic placeholder id; it never invents a URL.
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

  it("derives provider fields without owning provider endpoint rules", () => {
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
