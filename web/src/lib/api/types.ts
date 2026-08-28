// Wire types for the faces the console talks to. Admin-plane shapes mirror
// contracts/openapi.generated.json (schemars SSOT); managed shapes mirror the
// Managed Agents wire implemented by awaken-protocol-managed.

// ---- admin config plane ----

import type * as Contract from "../../../../contracts/model-config";
import type { ModelSelection } from "../../../../contracts/model-selection.generated";
export type { ModelSelection } from "../../../../contracts/model-selection.generated";

export type {
  ApiDialect,
  CatalogSyncResult,
  ConfigCapabilitiesView,
  ExecutableModelOption,
  ModelAttributes,
  ModelTarget,
  Offering,
  ProtocolEndpoint,
  Provider,
  ProviderCatalog,
  ProviderConnectionView,
  ProviderConnectionSummary,
  ProviderDriverDescriptor,
} from "./model-types";

export type CredentialBinding = Contract.CredentialBinding;
export type CredentialSource = Contract.CredentialSourceView;
export type CredentialPoolMember = Contract.CredentialPoolMember;
export type CredentialPool = Contract.CredentialPool;
export type CredentialValidation = Contract.CredentialValidation;
export type ResolvedInferenceView = Contract.ResolvedInferenceView;
export type InferenceProfile = Contract.InferenceProfile;
export type ProfileCandidate = Contract.PrimaryElement;
export type ResolvedCandidatesView = Contract.ResolvedCandidatesView;

// ---- IAM (embedded) ----

export interface IamTokenView {
  id: string;
  prefix?: string;
  principal_id?: string;
  role?: string;
  workspace_id?: string;
  created_at?: string;
  expires_at?: string;
  revoked_at?: string;
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
  multiagent?: MultiagentConfig | null;
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
  environment_id?: string | null;
  created_at: string;
  updated_at: string;
  archived_at?: string | null;
  title?: string | null;
  metadata: Record<string, string>;
  resources: unknown[];
  outcome_evaluations: unknown[];
  status: "running" | "idle" | "rescheduling" | "terminated";
  preparation?: {
    status: "preparing" | "ready" | "failed";
    error?: string;
  };
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
  agent: string
    | { id: string; type: "agent"; version?: number }
    | { id: string; type: "agent_with_overrides"; version?: number; model?: string };
  environment_id?: string;
  title?: string;
  metadata?: Record<string, string>;
  mcp_servers?: { name: string; url: string; prompts_as_skills?: boolean }[];
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
export type { FileArtifact, FileListResponse } from "./file-types";

// ---- environments ----

export type EnvNetworking =
  | { type: "unrestricted" }
  | { type: "limited"; allowed_hosts?: string[]; allow_package_managers?: boolean; allow_mcp_servers?: boolean };
export interface EnvironmentPackages {
  type?: "packages";
  apt?: string[];
  cargo?: string[];
  gem?: string[];
  go?: string[];
  npm?: string[];
  pip?: string[];
}

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
/** An Awaken sandbox-policy value projected to the backend `SandboxSpec`.
 * It is deliberately separate from the official Environment config union. */
export interface SandboxConfig {
  isolation?: "workdir" | "namespace" | "container";
  mounts?: SandboxMount[];
  network?: SandboxNetwork;
  limits?: { cpu_millis?: number | null; memory_bytes?: number | null };
}
export interface EnvironmentConfig {
  type: "cloud" | "self_hosted";
  networking?: EnvNetworking;
  packages?: EnvironmentPackages;
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

export type SandboxProvisioning = "eager" | "on_tool_use";
export interface SandboxExecutionPolicy {
  id: string;
  version: number;
  config: SandboxConfig;
  provisioning: SandboxProvisioning;
  disabled: boolean;
}
export interface SandboxPolicyBinding {
  environment_id: string;
  policy_id?: string;
  version?: number;
  provisioning: SandboxProvisioning;
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

// ---- Agent default inputs (ADR-0063) ---------------------------------------
export type InputResourceKind = "file" | "memory_store" | "repository";
export type ResourceAccess = "read_only" | "read_write";
export interface InputResourceId {
  kind: InputResourceKind;
  id: string;
}
export interface InputBinding {
  binding_id: string;
  target: InputResourceId;
  mount_path: string;
  access: ResourceAccess;
  instructions?: string | null;
}
export interface AgentInputConfig {
  agent_id: string;
  environment?: {
    environment_id: string;
    revision: number;
  } | null;
  inputs: InputBinding[];
  revision: number;
}

// ---- skills ----

export interface Skill {
  id: string;
  type?: string;
  name?: string;
  display_name?: string;
  display_title?: string | null;
  description?: string | null;
  latest_version?: string | number;
  source?: "custom" | "anthropic" | string;
  [k: string]: unknown;
}

export interface SkillVersionFile {
  path: string;
  size_bytes: number;
  executable: boolean;
}
export interface SkillVersion {
  id: string;
  type: "skill_version";
  skill_id: string;
  version: string;
  name: string;
  description: string;
  directory: string;
  files: string[];
  file_entries?: SkillVersionFile[];
  bundle_sha256: string;
  created_at: string;
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
  multiagent?: MultiagentConfig;
  metadata: Record<string, string>;
  version: number;
  status: "published" | "disabled" | "archived";
  disabled_at?: string | null;
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

/** The closed Managed Agents coordinator roster accepted by the config plane. */
export type MultiagentTarget =
  | string
  | { type: "agent"; id: string; version?: number }
  | { type: "self" };
export interface MultiagentConfig {
  type: "coordinator";
  agents: MultiagentTarget[];
}

/** Run-scoped safety budget frozen into each published Agent revision. */
export interface DelegationLimits {
  max_depth: number;
  max_parallel: number;
  max_total: number;
}

export interface AgentConfig {
  id: string;
  /** Optimistic-concurrency revision returned by the config plane. */
  generation?: number;
  type?: "agent";
  // managed Agent object fields:
  name?: string | null;
  description?: string | null;
  model: ModelSelection | { id: string; speed?: string } | string;
  system?: string;
  metadata?: Record<string, string>;
  tools: string[];
  mcp_servers: unknown[];
  skills: unknown[];
  multiagent?: MultiagentConfig | null;
  /** Omitted means the backend compatibility default (8 / 8 / 64). */
  delegation_limits?: DelegationLimits;
  /** Logical deployment Hand id; transport remains typed deployment config. */
  hand?: string | null;
  disabled_at?: string | null;
  archived_at?: string | null;
  // extensions (our differentiated value, additive to the managed object):
  max_steps: number;
  plugins: string[];
  /** Per-plugin config sections, keyed by plugin id (permission / state_machine /
   * generative-ui live here as extension-owned JSON). */
  plugin_config: Record<string, unknown>;
  context_policy: ContextPolicy;
  /** Exact model-facing appearance/exposure overrides, keyed by canonical tool id. */
  tool_overrides?: ToolOverride[];
  /** Ordered first-match policy for static ids and live namespaces. */
  tool_exposure?: ToolExposurePolicy;
  /** Provider-neutral discovery behavior for on-demand tool definitions. */
  tool_discovery?: ToolDiscoverySettings;
  /** Advanced persisted fields surfaced in the lossless JSON editor. */
  recovery_policies?: Record<string, unknown>;
  compaction?: unknown;
}
export type ToolExposure = "eager" | "on_demand";
export type ToolSelector =
  | { kind: "exact"; value: string }
  | { kind: "prefix"; value: string };
export interface ToolExposurePolicy {
  rules?: Array<{ selector: ToolSelector; exposure: ToolExposure }>;
  default?: ToolExposure;
}
export type ToolPromptInjection =
  | { mode: "automatic" }
  | { mode: "disabled" }
  | { mode: "custom"; text: string };
export interface ToolDiscoverySettings {
  max_results?: number;
  prompt?: ToolPromptInjection;
}
/** One exact tool override. `target` always remains the canonical id. */
export interface ToolOverride {
  target: string;
  alias?: string;
  description?: string;
  exposure?: ToolExposure;
}
/** A list/get item: the object plus a live `published` flag (a compiled config is
 * currently installed in the runtime catalog). */
export type AgentConfigItem = AgentConfig & { published?: boolean };
export interface AgentConfigList {
  data: AgentConfigItem[];
}

export interface SessionThreadAgent {
  id: string;
  type: "agent";
  version: number;
  model: string | { id: string };
  name: string;
  description?: string | null;
  tools: unknown[];
  mcp_servers: unknown[];
  skills: unknown[];
}
export interface SessionThreadUsage {
  cache_read_input_tokens?: number;
  input_tokens?: number;
  output_tokens?: number;
  cache_creation?: {
    ephemeral_1h_input_tokens?: number;
    ephemeral_5m_input_tokens?: number;
  };
}
export interface SessionThread {
  id: string;
  type: "session_thread";
  session_id: string;
  parent_thread_id: string | null;
  agent: SessionThreadAgent;
  created_at: string;
  updated_at: string;
  archived_at?: string | null;
  status: "running" | "idle" | "rescheduling" | "terminated";
  stats?: {
    active_seconds?: number;
    duration_seconds?: number;
    startup_seconds?: number;
  } | null;
  usage?: SessionThreadUsage | null;
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
  source_revision: number;
  installed: boolean;
}

export type {
  Capabilities,
  PluginCap,
  PolicyCap,
  RuntimeCap,
  SandboxCapability,
  SandboxPreset,
  ToolCap,
} from "./capability-types";

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

export type { Vault, VaultCredential, VaultCredentialAuth } from "./vault-types";
export type { MemoryEntry, MemoryStore, MemoryVersion } from "./memory-types";
export type {
  Dream,
  DreamCapability,
  DreamInput,
  DreamModelConfig,
  DreamOutput,
  DreamPage,
  DreamPolicy,
  DreamPolicyConfig,
  DreamStatus,
  DreamUsage,
  ManagedModel,
  ManagedModelPage,
  PlatformCapabilities,
} from "./dream-types";
