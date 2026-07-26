import { describe, expect, it } from "vitest";
import type { ProviderDriverDescriptor } from "../lib/api/types";
import { providerDraftDefaults } from "./models";

// UI cause/effect rules kept beside the executable tests:
// T1: C1 descriptor has a preferred endpoint -> E1 form uses its protocol, URL,
// and stable endpoint id. T2: C2 descriptor has no fixed endpoint -> E2 form
// stays editable with a deterministic placeholder id; it never invents a URL.

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
