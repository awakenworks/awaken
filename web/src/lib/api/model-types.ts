// Model-management wire types mirror contracts/openapi.generated.json.

export interface Provider {
  id: string;
  slug: string;
  display_name: string;
  version: number;
}

export type ApiDialect =
  | "anthropic_messages"
  | "open_ai_chat"
  | "open_ai_responses"
  | "gemini"
  | "vertex_gemini";

export interface ProtocolEndpoint {
  id: string;
  provider_id: string;
  dialect: ApiDialect;
  base_url?: string | null;
  timeout_secs?: number;
  display_name: string;
  version: number;
}

export interface Offering {
  model_id: string;
  provider_id: string;
  protocol_endpoint_id: string;
  dialect: ApiDialect;
  upstream_model?: string | null;
  source?: "manual" | "provider_api";
  status?: "active" | "unavailable";
  last_seen_at_unix_ms?: number | null;
}

export interface CatalogSyncResult {
  discovered: number;
  activated: number;
  marked_unavailable: number;
  observed_at_unix_ms: number;
}

export interface ModelTarget {
  model_id: string;
  provider_id?: string | null;
  protocol_endpoint_id?: string | null;
}

/** Intrinsic per-model_id attributes published by the control plane. */
export interface ModelAttributes {
  context_window?: number | null;
  max_output_tokens?: number | null;
  provenance?: Record<
    string,
    { source: "manual" | "provider_api" | "curated"; observed_at_unix_ms: number }
  >;
}

export interface ProviderCatalog {
  providers: Record<string, Provider>;
  endpoints: Record<string, ProtocolEndpoint>;
  offerings: Offering[];
  model_attributes?: Record<string, ModelAttributes>;
}

export interface EnvironmentProviderProposal {
  provider_id: string;
  endpoint_id: string;
  dialect: ApiDialect;
  base_url?: string;
  model_id?: string;
  credential_env: string;
  credential_present: boolean;
}
