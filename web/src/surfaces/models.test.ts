import { describe, expect, it } from "vitest";
import type { CredentialSource, ProviderDriverDescriptor } from "../lib/api/types";
import { cloudModelUiState, modelCatalogPresentation } from "./model-cloud-capability";
import { modelTestAgentConfig, modelTestAgentId } from "./models";
import {
  providerConfigurationDefaults,
  providerDraftDefaults,
  credentialReusableForProvider,
} from "./provider-connection-panel";
import { visibleAgents } from "../lib/visible-agents";

// UI cause/effect rules kept beside the executable tests:
// T1: C1 descriptor has a preferred endpoint -> E1 form uses its protocol and URL;
// endpoint identity remains server-derived. T2: C2 descriptor has no fixed endpoint
// -> E2 the URL stays empty and the dialect remains selectable.
// Cloud capability decision table:
// T7 unknown capabilities -> E7 no cloud action while loading.
// T8 cloud models off (regardless of Cloud login) -> E8 local/BYOK-only UI.
// T9 cloud models on + unauthenticated/authenticated -> E9 sign-in-required/ready.
// T10 hosted managed supply -> E10 read-only managed UI; no refresh or BYOK form.
// Catalog disclosure/action decision table:
// T11 managed supply -> E11 hide Endpoint/dialect/source and session test because
// those are Cloud operations concerns. T12 every other supply mode -> E12 retain
// the local configuration detail and its real session round-trip test.

const openai: ProviderDriverDescriptor = {
  provider_kind: "openai",
  display_name: "OpenAI",
  supported_dialects: ["open_ai_responses", "open_ai_chat"],
  auth_methods: ["api_key"],
  configuration_fields: [],
  default_endpoints: [
    {
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
      endpointName: "",
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
      endpointName: "",
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

describe("provider credential compatibility", () => {
  const credential = (kind: CredentialSource["kind"], provider_id: string | null = "openai") => ({
    id: "cred_1",
    workspace_id: "workspace",
    kind,
    provider_id,
    status: "active",
    version: 1,
  }) as CredentialSource;

  it("offers only materializable credentials supported by the selected provider", () => {
    expect(credentialReusableForProvider(credential("vault"), openai)).toBe(true);
    expect(credentialReusableForProvider(credential("worker_local"), openai)).toBe(false);
    expect(credentialReusableForProvider(credential("env"), openai)).toBe(false);
    expect(credentialReusableForProvider(credential("oauth"), openai)).toBe(false);
    expect(credentialReusableForProvider(credential("vault", "anthropic"), openai)).toBe(false);
    expect(credentialReusableForProvider(credential("vault", null), openai)).toBe(false);
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
      profile_authoring_enabled: true,
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

  it("projects hosted supply independently from an interactive Cloud login", () => {
    const hosted = capabilities(true, false);
    hosted.models.local_catalog_enabled = false;
    hosted.models.byok_enabled = false;
    hosted.models.profile_authoring_enabled = false;
    expect(cloudModelUiState(hosted)).toBe("managed");
  });

  it("keeps hosted infrastructure and invalid session tests out of the user catalog", () => {
    expect(modelCatalogPresentation("managed")).toEqual({
      showSupplyInfrastructure: false,
      allowSessionTest: false,
    });
    for (const state of ["loading", "local", "sign_in_required", "ready"] as const) {
      expect(modelCatalogPresentation(state)).toEqual({
        showSupplyInfrastructure: true,
        allowSessionTest: true,
      });
    }
  });
});

describe("live model test publication", () => {
  it("creates a deterministic hidden Agent pinned to the selected model", () => {
    expect(modelTestAgentId("deepseek-e2e")).toBe(modelTestAgentId("deepseek-e2e"));
    expect(modelTestAgentId("deepseek-e2e")).not.toBe(modelTestAgentId("other"));
    expect(modelTestAgentConfig("deepseek-e2e")).toMatchObject({
      id: modelTestAgentId("deepseek-e2e"),
      model: { id: "deepseek-e2e" },
      metadata: { "awaken.internal": "model-test" },
    });
  });

  it("does not expose internal model-test publications in Agent choices", () => {
    expect(visibleAgents([
      modelTestAgentConfig("deepseek-e2e"),
      { ...modelTestAgentConfig("other"), id: "human-agent", metadata: {} },
    ])).toHaveLength(1);
  });
});
