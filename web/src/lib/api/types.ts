// Wire types for the faces the console talks to. Admin-plane shapes mirror
// contracts/openapi.generated.json (schemars SSOT); managed shapes mirror the
// Managed Agents wire implemented by awaken-protocol-managed.

// ---- admin config plane ----

import type * as Contract from "../../../../contracts/model-config";
import type { ModelSelection } from "../../../../contracts/model-selection.generated";
import type {
  BetaManagedAgentsEventParams,
  BetaManagedAgentsSendSessionEvents,
  BetaManagedAgentsSession,
  BetaManagedAgentsSessionEvent,
  BetaManagedAgentsSessionEventsPageCursor,
  BetaManagedAgentsSessionResource,
  BetaManagedAgentsSessionResourcesPageCursor,
  BetaManagedAgentsSessionsBidirectionalPageCursor,
  BetaManagedAgentsSessionThread,
  BetaManagedAgentsSessionThreadsPageCursor,
  BetaManagedAgentsSessionThreadUsage,
  BetaManagedAgentsSessionUsage,
  ManagedSessionContentBlock,
  SessionCreateParams,
  SessionUpdateParams,
} from "@awaken/managed-sdk-oracle/current-types";
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

/** Awaken control-plane collection shape; Managed pages use SDK-specific aliases below. */
export interface Page<T> {
  data: T[];
  has_more: boolean;
  next_page: string | null;
}

/** Managed CRUD collections use Anthropic's opaque cursor shape. */
export interface CursorPage<T> {
  data: T[];
  next_page: string | null;
}

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

export interface IamTokenMintResponse {
  token?: string;
  secret?: string;
  cleartext?: string;
  value?: string;
  api_token?: IamTokenView;
}

// ---- managed sessions ----

/** Exact current official Managed wire types, selected by the SDK oracle. */
export type Session = BetaManagedAgentsSession;
export type SessionAgent = BetaManagedAgentsSession["agent"];
export type SessionUsage = BetaManagedAgentsSessionUsage;
export type SessionEvent = BetaManagedAgentsSessionEvent;
export type InboundEvent = BetaManagedAgentsEventParams;
export type SendEventsResponse = BetaManagedAgentsSendSessionEvents;
export type ListEventsResponse = Pick<
  BetaManagedAgentsSessionEventsPageCursor,
  "data" | "next_page"
>;
export type ListSessionsResponse = Pick<
  BetaManagedAgentsSessionsBidirectionalPageCursor,
  "data" | "next_page" | "prev_page"
>;
export type SessionThread = BetaManagedAgentsSessionThread;
export type SessionThreadUsage = BetaManagedAgentsSessionThreadUsage;
export type ListSessionThreadsResponse = Pick<
  BetaManagedAgentsSessionThreadsPageCursor,
  "data" | "next_page"
>;
export type SessionResource = BetaManagedAgentsSessionResource;
export type ListSessionResourcesResponse = Pick<
  BetaManagedAgentsSessionResourcesPageCursor,
  "data" | "next_page"
>;
export type UpdateSessionRequest = Omit<SessionUpdateParams, "betas">;
export type CreateSessionRequest = Omit<SessionCreateParams, "betas">;
export type ContentBlock = ManagedSessionContentBlock;
export type { FileArtifact, FileListResponse } from "./file-types";

export type {
  Environment,
  EnvironmentConfig,
  EnvironmentPackages,
  EnvNetworking,
  SandboxConfig,
  SandboxExecutionPolicy,
  SandboxMount,
  SandboxNetwork,
  SandboxPolicyBinding,
  SandboxProvisioning,
  WorkQueueStats,
} from "./environment-types";

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

export interface InferenceOptions {
  effort?: "low" | "medium" | "high" | "xhigh" | "max";
  speed?: "standard" | "fast";
  inference_geo?: "us" | "eu" | "apac" | "cn" | "jp" | "au" | "ca" | "uk" | "hk";
}

export interface CompactionStrategy {
  /** Omit to derive the trigger from the selected model's usable context window. */
  window?: number;
  /** Omit to use the runtime's safe recent-turn default. */
  keep_recent?: number;
}

export interface ModelCandidate {
  provider_identity_ref: string;
  model_ref: string;
  backend_ref: string;
}

export interface CustomClientTool {
  type: "custom";
  name: string;
  description: string;
  input_schema: Record<string, unknown>;
}

export interface AgentMcpHttpServer {
  type?: "url";
  name: string;
  url: string;
  credential?: { id: string; revision: number };
  prompts_as_skills?: boolean;
}

export interface AgentMcpSandboxServer {
  type: "sandbox_stdio";
  name: string;
  command: string;
  args?: string[];
  env?: Record<string, string>;
  prompts_as_skills?: boolean;
}

export type AgentMcpServer = AgentMcpHttpServer | AgentMcpSandboxServer;

export interface AgentToolsetConfig {
  name: string;
  enabled?: boolean;
  permission_policy?: { type: "always_allow" | "always_ask" };
  type?: string;
  allowed_domains?: string[];
  blocked_domains?: string[];
  max_content_tokens?: number;
  user_location?: Record<string, string>;
}

export interface AgentToolset {
  type: "agent_toolset_20260401" | "mcp_toolset";
  mcp_server_name?: string;
  configs?: AgentToolsetConfig[];
  default_config?: {
    enabled?: boolean;
    permission_policy?: { type: "always_allow" | "always_ask" };
  };
}

export type AgentTool = string | CustomClientTool | AgentToolset;

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
  tools: AgentTool[];
  inference?: InferenceOptions;
  model_candidates?: ModelCandidate[];
  tool_patterns?: string[];
  mcp_servers: AgentMcpServer[];
  skills: unknown[];
  multiagent?: MultiagentConfig | null;
  /** Omitted means the backend compatibility default (8 / 8 / 64). */
  delegation_limits?: DelegationLimits;
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
  compaction?: CompactionStrategy | null;
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
  ManagedToolsetCap,
  PluginCap,
  PolicyCap,
  RuntimeCap,
  SandboxCapability,
  SandboxPreset,
  ToolCap,
} from "./capability-types";

export type { PermissionBehavior, PermissionConfig, PermissionRuleConfig } from "./permission-types";

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
