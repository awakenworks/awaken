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
}
/** Intrinsic per-model_id attributes the control plane publishes (context window
 * feeds the compaction budget). Keyed by model_id in the catalog. */
export interface ModelAttributes {
  context_window?: number | null;
  max_output_tokens?: number | null;
}
export interface ProviderCatalog {
  providers: Record<string, Provider>;
  endpoints: Record<string, ProtocolEndpoint>;
  offerings: Offering[];
  model_attributes?: Record<string, ModelAttributes>;
}

export type ApiDialect = "anthropic_messages" | "open_ai_chat" | "gemini";

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
  /** A bare id or a `{ id }` object, depending on how the agent was authored. */
  model?: string | { id: string };
  name?: string;
  tools?: unknown[];
  mcp_servers?: unknown[];
  skills?: unknown[];
  multiagent?: unknown;
}
/** Accumulated token usage for a session (zero until the first turn commits). */
export interface SessionUsage {
  input_tokens: number;
  output_tokens: number;
  cache_read_input_tokens: number;
  cache_creation_input_tokens: number;
}

export interface Session {
  id: string;
  type: "session";
  agent: SessionAgent;
  usage?: SessionUsage;
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
  | { type: "session.error"; error?: { type?: string; message?: string }; message?: string }
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
  | { type: "user.interrupt" };

export interface EventReceipt {
  id: string;
  type: string;
  processed_at?: string | null;
}

export interface SendEventsResponse {
  data: EventReceipt[];
}

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

// ---- Managed Agents SDK page shape (PageCursor: {data, has_more, next_page}) ----

export interface Page<T> {
  data: T[];
  has_more: boolean;
  next_page: string | null;
}

/** A resource mounted for a session (ADR-0038): the SDK-shaped item `/v1/sessions/:id/
 * resources` returns — a memory store / file / repo / Skill attached to the sandbox. */
export interface SessionResourceDto {
  id: string;
  type: string; // "file" | "memory_store" | "github_repository" | "skill"
  mount_path: string;
  memory_store_id?: string;
  file_id?: string;
  url?: string;
  resource_id?: string;
  instructions?: string;
}
/** An output artifact a session produced — a file the agent wrote under `outputs/`,
 * harvested into the blob store (`GET /v1/files?scope_id=<session>`). */
export interface FileArtifact {
  id: string;
  type: string;
  filename: string;
  downloadable?: boolean;
}

// ---- environments ----

export type EnvNetworking =
  | { type: "unrestricted" }
  | { type: "limited"; allowed_hosts?: string[]; allow_package_managers?: boolean; allow_mcp_servers?: boolean };

/** A sandbox mount made visible inside the isolated worker. */
export interface SandboxMount {
  mount_path: string;
  access: "read_only" | "read_write";
}
/** Egress policy: full, deny-by-default allowlist, or none. */
export type SandboxNetwork =
  | { mode: "unrestricted" }
  | { mode: "allowlist"; hosts?: string[] }
  | { mode: "none" };
/** The `config.sandbox` an environment persists — a UI projection of the backend
 * `SandboxSpec` (see /v1/capabilities `sandbox.config_schema`). */
export interface SandboxConfig {
  isolation?: "workdir" | "namespace" | "container";
  mounts?: SandboxMount[];
  network?: SandboxNetwork;
  limits?: { cpu_millis?: number | null; memory_bytes?: number | null };
}
export interface EnvironmentConfig {
  type: "cloud" | "self_hosted";
  networking?: EnvNetworking;
  /** Execution backend: "awaken" (native) or "acp:<cli>". Absent = native. */
  runtime?: string;
  /** Present when the worker runs in an isolated sandbox. */
  sandbox?: SandboxConfig;
}
export interface Environment {
  id: string;
  type: "environment";
  name: string;
  description?: string;
  config: EnvironmentConfig;
  metadata: Record<string, string>;
  archived_at?: string | null;
  created_at: string;
  updated_at: string;
}

/** An environment's durable work queue state (GET /v1/environments/:id/work/stats):
 * `depth` = items queued (waiting to be claimed), `pending` = items a worker has
 * claimed and is processing. */
export interface WorkQueueStats {
  type: "work_queue_stats";
  depth: number;
  pending: number;
  oldest_queued_at?: string | null;
  workers_polling: number;
}

// ---- memory stores ----

export interface MemoryStore {
  id: string;
  type: "memory_store";
  name: string;
  description?: string | null;
  metadata: Record<string, string>;
  archived_at?: string | null;
  created_at: string;
  updated_at: string;
}

// ---- agent resource bindings (ADR-0038): which resources an agent mounts ----
export type ResourceKind = "outputs" | "file" | "memory_store" | "github_repository" | "skill";
export type ResourceAccess = "read_only" | "read_write";
export interface ResourceBinding {
  kind: ResourceKind;
  /** The backing resource id (a memory store id, file id, repo URL; empty for outputs). */
  resource_id?: string;
  /** Where it appears in the sandbox. */
  mount_path: string;
  access: ResourceAccess;
  instructions?: string | null;
}
export interface AgentResourceConfig {
  agent_id: string;
  resources: ResourceBinding[];
  version: number;
}

// ---- skills ----

export interface Skill {
  id: string;
  type?: string;
  name?: string;
  display_name?: string;
  description?: string | null;
  latest_version?: string | number;
  [k: string]: unknown;
}

// ---- deployments ----

export interface DeploymentSchedule {
  type: "cron";
  expression: string;
  timezone: string;
  upcoming_runs_at?: string[];
  last_run_at?: string | null;
}
export interface Deployment {
  id: string;
  type?: "deployment";
  name: string;
  agent: { id: string; type?: "agent"; version?: number };
  environment_id: string;
  schedule: DeploymentSchedule;
  status?: string;
  paused_reason?: { type: string } | null;
  archived_at?: string | null;
  created_at: string;
}
export interface DeploymentRun {
  id: string;
  deployment_id: string;
  session_id?: string | null;
  error?: { type: string; message?: string } | null;
  trigger_context?: { type: string };
  created_at: string;
}

// ---- agents (registry) ----

export interface AgentModelRef {
  id: string;
  speed?: string;
}
export interface Agent {
  id: string;
  type: "agent";
  name: string;
  model: AgentModelRef | string;
  system?: string | null;
  description?: string | null;
  tools: unknown[];
  mcp_servers: unknown[];
  skills: unknown[];
  multiagent?: unknown;
  metadata: Record<string, string>;
  version: number;
  archived_at?: string | null;
  created_at: string;
  updated_at: string;
}

// ---- config-plane agent authoring (/v1/config/agents) ----
// The object model is the managed `/v1/agents` Agent object (name / model / system
// / tools / mcp_servers / skills / multiagent / metadata) PLUS our extension block
// (plugins / plugin_config / context_policy / max_steps). The console authors this
// against our own config plane; `publish` compiles + installs it so sessions run it.

/** Internally tagged on `kind` (snake_case), mirroring the Rust ContextPolicy. */
export type ContextPolicy = { kind: "keep_all" } | { kind: "keep_last"; keep_last: number };

export interface AgentConfig {
  id: string;
  type?: "agent";
  // managed Agent object fields:
  name?: string | null;
  description?: string | null;
  model: { id: string; speed?: string } | string;
  system?: string;
  metadata?: Record<string, string>;
  tools: string[];
  mcp_servers: unknown[];
  skills: unknown[];
  multiagent?: unknown;
  // extensions (our differentiated value, additive to the managed object):
  max_steps: number;
  plugins: string[];
  /** Per-plugin config sections, keyed by plugin id (permission / state_machine /
   * deferred-tools / generative-ui all live here as JSON). */
  plugin_config: Record<string, unknown>;
  context_policy: ContextPolicy;
  /** Per-tool model-facing presentation (ADR-0053): alias / description override /
   * defer, keyed by canonical tool id (a catalog id or an MCP `mcp__server__tool`). */
  tool_overrides?: ToolOverride[];
  /** Advanced persisted fields surfaced in the lossless JSON editor. */
  recovery_policies?: Record<string, unknown>;
  compaction?: unknown;
}
/** One tool's presentation override. `target` is the canonical tool id; `alias`
 * renames it for the model, `description` replaces its text, `defer` withholds its
 * schema until the model loads it via `tool_open`. */
export interface ToolOverride {
  target: string;
  alias?: string;
  description?: string;
  defer?: boolean;
}
/** A list/get item: the object plus a live `published` flag (a compiled config is
 * currently installed in the runtime catalog). */
export type AgentConfigItem = AgentConfig & { published?: boolean };
export interface AgentConfigList {
  data: AgentConfigItem[];
}
/** One validation problem from `/validate`, field-routed by the config domain (compile).
 * `path` is the config field the issue is about (`""` = whole config). */
export interface ValidationIssue {
  path: string;
  message: string;
  severity?: string;
}
export interface ValidationResult {
  valid: boolean;
  issues: ValidationIssue[];
}
export interface PublishResult {
  publication_id: string;
  fingerprint: string;
  agent_id: string;
  installed: boolean;
}

// ---- capabilities (GET /v1/capabilities) ----
// Host-level facts the editor authors data-driven: tool descriptors (with their
// JSON-Schema params) and installable plugins (with per-plugin config schema).
export interface ToolCap {
  id: string;
  description: string;
  parameters: Record<string, unknown>;
}
export interface PluginCap {
  id: string;
  config_sections: string[];
  config_schema: Record<string, unknown>;
}
/** An always-on policy (not an installable plugin) whose `plugin_config` section
 * shapes a run — e.g. the permission gate. Rendered by a dedicated editor. */
export interface PolicyCap {
  id: string;
  config_section: string;
  config_schema: Record<string, unknown>;
}
/** An execution backend an environment can bind (the "where/how it runs" axis).
 * `awaken` = native; `acp:<cli>` routes to an ACP CLI adapter. */
export interface RuntimeCap {
  id: string;
  label: string;
  kind: "native" | "acp";
  cli?: string | null;
  description: string;
}
/** A one-click sandbox starting point over `sandbox.config_schema`. */
export interface SandboxPreset {
  id: string;
  label: string;
  description: string;
  spec: SandboxConfig;
}
export interface SandboxCapability {
  config_schema: Record<string, unknown>;
  presets: SandboxPreset[];
}
export interface Capabilities {
  runtime_version: string;
  tools: ToolCap[];
  plugins: PluginCap[];
  policies?: PolicyCap[];
  runtimes?: RuntimeCap[];
  sandbox?: SandboxCapability;
}

// ---- permission policy (the `permission` plugin_config section) ----
export type PermissionBehavior = "allow" | "ask" | "deny";
export interface PermissionRuleConfig {
  pattern: string;
  behavior: PermissionBehavior;
}
export interface PermissionConfig {
  default_behavior?: PermissionBehavior;
  mode?: string;
  rules?: PermissionRuleConfig[];
}

// ---- vaults ----

export interface Vault {
  id: string;
  type: "vault";
  display_name?: string;
  archived_at?: string | null;
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
