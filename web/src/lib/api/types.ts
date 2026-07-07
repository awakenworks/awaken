// Wire types for the faces the console talks to. Admin-plane shapes mirror
// contracts/openapi.generated.json (schemars SSOT); managed shapes mirror the
// Managed Agents wire implemented by awaken-protocol-managed.

// ---- admin config plane ----

export interface Provider {
  id: string;
  slug: string;
  display_name: string;
  version: number;
}
export interface ProtocolEndpoint {
  id: string;
  provider_id: string;
  flavor: ModelApiCompat;
  base_url?: string | null;
  timeout_secs?: number;
  display_name: string;
  version: number;
}
export interface Offering {
  model_id: string;
  provider_id: string;
  protocol_endpoint_id: string;
  flavor: ModelApiCompat;
  upstream_model?: string | null;
}
export interface ProviderCatalog {
  providers: Record<string, Provider>;
  endpoints: Record<string, ProtocolEndpoint>;
  offerings: Offering[];
}

export type ModelApiCompat = "anthropic_messages" | "open_ai_chat" | "gemini";

export type CredentialBinding =
  | { type: "none" }
  | { type: "exact"; credential_source_id: string }
  | { type: "one_of_credential_pool"; credential_pool_id: string };

export interface CredentialSource {
  id: string;
  workspace_id: string;
  kind: string;
  provider_id?: string | null;
  env_key?: string | null;
  status: string;
  version: number;
}
export interface CredentialPoolMember {
  credential_source_id: string;
  ordinal: number;
  enabled: boolean;
  selection_weight: number;
}
export interface CredentialPool {
  id: string;
  workspace_id: string;
  members: CredentialPoolMember[];
}
export interface CredentialValidation {
  status: "valid" | "invalid" | "unknown";
  adapter_kind: string;
}
export interface ResolvedInferenceView {
  model_id: string;
  provider_id: string;
  protocol_endpoint_id: string;
  adapter_kind: string;
  base_url?: string;
  credential_present: boolean;
}
export interface ResolvedMcpServerView {
  name: string;
  url: string;
  credential_present: boolean;
}
export interface InferenceProfile {
  model_id: string;
  credential_binding: CredentialBinding;
  disabled_endpoint_ids: string[];
}
export interface McpServerDef {
  id: string;
  display_name: string;
  url: string;
  credential_binding: CredentialBinding;
  version: number;
}
export interface AgentMcpConfig {
  agent_id: string;
  mcp_server_ids: string[];
  version: number;
}
export interface Project {
  id: string;
  workspace_id: string;
  display_name: string;
  version: number;
}
export interface ProjectAgentConfig {
  project_id: string;
  agent_id: string;
  mcp_server_ids: string[];
  version: number;
}

// ---- IAM (embedded) ----

export interface IamTokenView {
  id: string;
  name?: string;
  role?: string;
  workspace_id?: string;
  [k: string]: unknown;
}

// ---- managed sessions ----

export interface SessionAgent {
  id: string;
  type: "agent";
  version?: number;
  model?: string;
  name?: string;
  tools?: unknown[];
  mcp_servers?: unknown[];
  skills?: unknown[];
  multiagent?: unknown;
}
export interface Session {
  id: string;
  type: "session";
  agent: SessionAgent;
  /** Extension: the project ingress the session was created through. */
  project_id?: string | null;
  environment_id?: string | null;
  created_at: string;
  updated_at: string;
  archived_at?: string | null;
  title?: string | null;
  metadata: Record<string, string>;
  resources: unknown[];
  outcome_evaluations: unknown[];
  status: string;
}

export interface ContentBlockText {
  type: "text";
  text: string;
}
export type ContentBlock = ContentBlockText | { type: string; [k: string]: unknown };

export type StopReason =
  | { type: "end_turn" }
  | { type: "requires_action"; event_ids: string[] }
  | { type: "retries_exhausted" };

export type OutboundKind =
  | { type: "agent.message"; content: ContentBlock[] }
  | {
      type: "agent.tool_use";
      name: string;
      input: unknown;
      evaluated_permission?: string;
    }
  | { type: "agent.tool_result"; tool_use_id: string; content: ContentBlock[]; is_error?: boolean }
  | { type: "agent.custom_tool_use"; name: string; input: unknown }
  | { type: "session.status_running" }
  | { type: "session.status_idle"; stop_reason: StopReason }
  | { type: "span.outcome_evaluation_start"; outcome_id: string; iteration: number }
  | {
      type: "span.outcome_evaluation_end";
      outcome_id: string;
      iteration: number;
      result: string;
      explanation?: string;
    }
  | { type: string; [k: string]: unknown };

export type SessionEvent = { id: string; processed_at?: string | null } & OutboundKind;

export interface ListEventsResponse {
  data: SessionEvent[];
  next_page: string | null;
  has_more: boolean;
}

export type InboundEvent =
  | { type: "user.message"; content: ContentBlock[]; model?: string }
  | {
      type: "user.tool_confirmation";
      tool_use_id: string;
      result: "allow" | "deny";
      deny_message?: string;
    }
  | { type: "user.custom_tool_result"; custom_tool_use_id: string; content?: ContentBlock[]; is_error?: boolean }
  | { type: "user.define_outcome"; description: string; rubric: string; max_iterations?: number }
  | { type: "user.interrupt" }
  | { type: "user.pause" }
  | { type: "user.resume" };

export interface ListSessionsResponse {
  data: Session[];
  next_page: string | null;
  has_more: boolean;
}

/** POST /v1/sessions/{id} — the session's client-mutable fields. */
export interface UpdateSessionRequest {
  title?: string | null;
  metadata?: Record<string, string>;
}

export interface CreateSessionRequest {
  agent: string | { id: string; version?: number; model?: string };
  environment_id?: string;
  title?: string;
  metadata?: Record<string, string>;
  mcp_servers?: { name: string; url: string }[];
  vault_ids?: string[];
}

// ---- vaults ----

export interface Vault {
  id: string;
  type: "vault";
  [k: string]: unknown;
}
/** The secret-free credential projection: `type` is the object type
 * ("vault_credential"); the credential kind + fields are under `auth`. */
export interface VaultCredentialAuth {
  type: "environment_variable" | "static_bearer" | "mcp_oauth";
  mcp_server_url?: string;
  secret_name?: string;
  expires_at?: string;
  [k: string]: unknown;
}
export interface VaultCredential {
  id: string;
  type: string;
  auth?: VaultCredentialAuth;
  display_name?: string;
  [k: string]: unknown;
}
