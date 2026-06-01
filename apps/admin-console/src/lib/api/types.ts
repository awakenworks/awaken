export type ContextCompactionMode = "keep_recent_raw_suffix" | "compact_to_safe_frontier";
export type CompactionExecutionMode = "off" | "background";
export type CompactionRawRetention = "preserve_durable";

export interface CompactionConfig {
  mode?: CompactionExecutionMode;
  summarizer_system_prompt: string;
  summarizer_user_prompt: string;
  summary_max_tokens?: number | null;
  summary_model?: string | null;
  min_savings_ratio: number;
  raw_retention?: CompactionRawRetention;
}

export interface ContextWindowPolicy {
  max_context_tokens: number;
  max_output_tokens: number;
  min_recent_messages: number;
  enable_prompt_cache: boolean;
  autocompact_threshold?: number | null;
  compaction_mode?: ContextCompactionMode;
  compaction_raw_suffix_messages?: number;
}

export interface RemoteAuth {
  type: string;
  [key: string]: unknown;
}

export interface RemoteEndpoint {
  backend?: string;
  base_url: string;
  auth?: RemoteAuth | null;
  target?: string | null;
  timeout_ms?: number;
  options?: Record<string, unknown>;
}

export interface AgentSpec {
  id: string;
  model_id: string;
  system_prompt: string;
  max_rounds?: number;
  max_continuation_retries?: number;
  stop_conditions?: StopConditionSpec[];
  context_policy?: ContextWindowPolicy | null;
  plugin_ids?: string[];
  /** Runtime hook filter — only hooks from listed plugins run (`[]` = no filter). */
  active_hook_filter?: string[];
  sections?: Record<string, unknown>;
  allowed_tools?: string[] | null;
  allowed_tool_patterns?: string[] | null;
  excluded_tools?: string[] | null;
  excluded_tool_patterns?: string[] | null;
  endpoint?: RemoteEndpoint | null;
  delegates?: string[];
  reasoning_effort?: string | number | null;
  /** Registry source. `undefined` = locally defined; otherwise registry name. */
  registry?: string | null;
  created_at?: number;
  updated_at?: number;
}

export type StopConditionSpec =
  | { type: "max_rounds"; rounds: number }
  | { type: "timeout"; seconds: number }
  | { type: "token_budget"; max_total: number }
  | { type: "consecutive_errors"; max: number }
  | { type: "stop_on_tool"; tool_name: string }
  | { type: "content_match"; pattern: string }
  | { type: "loop_detection"; window: number };

/** Default values mirroring `ContextWindowPolicy::default()` on the Rust side. */
export const DEFAULT_CONTEXT_POLICY: ContextWindowPolicy = {
  max_context_tokens: 200_000,
  max_output_tokens: 16_384,
  min_recent_messages: 10,
  enable_prompt_cache: true,
  autocompact_threshold: null,
  compaction_mode: "keep_recent_raw_suffix",
  compaction_raw_suffix_messages: 2,
};

export const DEFAULT_COMPACTION_CONFIG: CompactionConfig = {
  mode: "background",
  summarizer_system_prompt:
    "You are a conversation summarizer. Preserve all key facts, decisions, tool results, and action items. Be concise but complete.",
  summarizer_user_prompt:
    "Update the cumulative conversation summary.\n\n<existing-summary>\n{previous_summary}\n</existing-summary>\n\n<new-conversation>\n{messages}\n</new-conversation>",
  summary_max_tokens: null,
  summary_model: null,
  min_savings_ratio: 0.3,
  raw_retention: "preserve_durable",
};

export interface ToolSpec {
  id: string;
  name: string;
  description: string;
  category?: string | null;
  parameters_schema?: unknown;
}

/**
 * Closed enum of input/output modalities a model can accept or produce.
 * Mirrors the Rust `awaken_contract::registry_spec::Modality` enum; serde
 * uses snake_case so the wire values match these literals exactly.
 */
export type Modality = "text" | "image" | "audio" | "video" | "pdf";

/**
 * Set of modalities a model accepts on input and produces on output.
 *
 * An empty/omitted list means "unspecified" — runtime treats the model's
 * modality set as catalog metadata only and performs no enforcement.
 */
export interface Modalities {
  input?: Modality[];
  output?: Modality[];
}

/**
 * Serializable model offering — addressing (id, provider, upstream model),
 * intrinsic capabilities (context window, max output tokens, modalities,
 * knowledge cutoff), and per-million-token pricing.
 *
 * Wire shape mirrors `awaken_contract::registry_spec::ModelSpec`. All
 * capability/pricing fields are optional; `created_at` / `updated_at`
 * are server-attached envelope metadata (epoch milliseconds).
 */
export interface ModelSpec {
  id: string;
  provider_id: string;
  upstream_model: string;

  /** Maximum context window in tokens. Must be `> 0` when set. */
  context_window?: number;
  /** Hard ceiling on a single response's output tokens. Must be `> 0` and
   *  `<= context_window` when both are set. */
  max_output_tokens?: number;
  /** Input/output modality sets. Duplicates within a list are rejected by
   *  the server validator. */
  modalities?: Modalities;
  /** ISO date string: `YYYY-MM` or `YYYY-MM-DD`. */
  knowledge_cutoff?: string;

  /** USD per million input tokens. Must be finite and `>= 0` when set. */
  input_token_price_per_million_usd?: number;
  /** USD per million output tokens. Must be finite and `>= 0` when set. */
  output_token_price_per_million_usd?: number;

  created_at?: number;
  updated_at?: number;
}

export interface ProviderSpec {
  id: string;
  adapter: string;
  api_key?: string;
  base_url?: string;
  timeout_secs?: number;
  /**
   * Adapter-specific extras. Recognised keys:
   *  - `credentials_kind`: how to interpret `api_key`. One of:
   *    - `"bearer"` (default) — `api_key` is a static OAuth bearer / API key.
   *    - `"service_account_json"` — `api_key` is full Google service-account
   *      JSON content; awaken signs JWTs and mints OAuth tokens automatically.
   *      Only valid with `adapter: "vertex"`.
   *  - `scopes`: optional array of OAuth scopes (defaults to
   *    `["https://www.googleapis.com/auth/cloud-platform"]` for Vertex).
   *  - `headers`: object of extra HTTP headers per request.
   *
   * Unknown keys are silently ignored by the runtime (forward-compat).
   */
  adapter_options?: Record<string, unknown>;
  created_at?: number;
  updated_at?: number;
}

export interface ProviderRecord extends Omit<ProviderSpec, "api_key"> {
  has_api_key?: boolean;
}

export interface ProviderTestResponse {
  ok: boolean;
  latency_ms: number;
  network_tested: boolean;
  error?: string;
}

export interface McpRestartPolicy {
  enabled?: boolean;
  max_attempts?: number;
  delay_ms?: number;
  backoff_multiplier?: number;
  max_delay_ms?: number;
}

export interface McpServerSpec {
  id: string;
  transport: "stdio" | "http";
  command?: string;
  args?: string[];
  url?: string;
  config?: Record<string, unknown>;
  timeout_secs?: number;
  env?: Record<string, string>;
  restart_policy?: McpRestartPolicy;
  created_at?: number;
  updated_at?: number;
}

export interface McpServerRecord extends Omit<McpServerSpec, "env"> {
  has_env?: boolean;
  env_keys?: string[];
}

export interface PluginInfo {
  id: string;
  config_schemas: Array<{
    key: string;
    schema: Record<string, unknown>;
    display_name?: string | null;
    description?: string | null;
    category?: string | null;
    editor?: string | null;
    default_value?: unknown;
    ui_schema?: Record<string, unknown> | null;
  }>;
}

export interface SkillArgumentInfo {
  name: string;
  description?: string | null;
  required: boolean;
}

export interface SkillInfo {
  id: string;
  name: string;
  description: string;
  allowed_tools: string[];
  when_to_use?: string | null;
  arguments: SkillArgumentInfo[];
  argument_hint?: string | null;
  user_invocable: boolean;
  model_invocable: boolean;
  model_override?: string | null;
  context: "inline" | "fork";
  paths: string[];
}

export interface ToolInfo {
  id: string;
  name: string;
  description: string;
  source?: { kind: "builtin" | "plugin" | "mcp"; id?: string };
}

export interface AdminAssistantToolInfo {
  id: string;
  label: string;
  description: string;
  visibility: "admin_assistant_only";
  selectable_by_agents: false;
  exposable_to_protocols: false;
  requires_confirmation: boolean;
}

export interface AdminAssistantCapability {
  id: string;
  enabled: boolean;
  disabled_reason?: string | null;
  model_id?: string | null;
  visibility: "admin_only";
  endpoint: string;
  prompt: {
    editable: boolean;
    storage: string;
    system_prompt_locked: boolean;
  };
  tools_locked: boolean;
  bound_tools: AdminAssistantToolInfo[];
}

export interface AdminAssistantConfig {
  id: string;
  policy_prompt: string;
  model_id?: string | null;
  revision?: number | null;
}

export interface Capabilities {
  agents: string[];
  tools: ToolInfo[];
  plugins: PluginInfo[];
  skills: SkillInfo[];
  models: ModelSpec[];
  providers: Array<{ id: string }>;
  admin_assistant?: AdminAssistantCapability;
  supported_adapters?: string[];
  namespaces: Array<{
    namespace: string;
    schema: Record<string, unknown>;
  }>;
}

export type CapabilitiesResult =
  | { kind: "ok"; capabilities: Capabilities }
  | { kind: "route_absent" }
  | { kind: "store_unavailable"; message?: string };

/** Wire-format mirror of `GET /v1/mcp-servers/:id/status` response. */
export interface McpServerStatusResponse {
  connected: boolean;
  last_error?: string | null;
  tools: Array<{ name: string; description?: string | null }>;
  consecutive_failures: number;
  /** Unix seconds, omitted when no attempt yet. */
  last_attempt_at?: number | null;
  /** Unix seconds, omitted when never succeeded. */
  last_success_at?: number | null;
  reconnecting: boolean;
  permanently_failed: boolean;
  /** HTTP session generation, incremented on session reset/reinitialize cycles. */
  session_generation?: number | null;
  /** Count of successful runtime re-creations since the server was first enabled. */
  transport_reconnect_count?: number;
  /** Unix seconds, omitted before first successful initialize. */
  last_init_at?: number | null;
}

/** Wire-format mirror of `GET /v1/system/info` response. */
export interface SystemInfo {
  version: string;
  /** Server-resolved tenant/workspace scope for the current admin request. */
  scope_id?: string;
  uptime_seconds: number;
  config_store_enabled: boolean;
  audit_log_enabled: boolean;
  runtime_stats_enabled: boolean;
}

/** Wire-format mirror of Rust `awaken_ext_observability::AgentRuntimeSnapshot`. */
export interface AgentRuntimeSnapshot {
  agent_id: string;
  window_seconds: number;
  bucket_window_seconds: number;
  bucket_count: number;
  inference_count: number;
  error_count: number;
  input_tokens: number;
  output_tokens: number;
  avg_inference_duration_ms: number;
  min_inference_duration_ms: number;
  max_inference_duration_ms: number;
  p50_inference_duration_ms: number;
  p95_inference_duration_ms: number;
  p99_inference_duration_ms: number;
  inference_duration_histogram?: Array<{
    upper_bound_ms: number | null;
    count: number;
  }>;
  suspensions: number;
  handoffs: number;
  delegations: number;
  tool_calls_by_tool: Array<{
    tool: string;
    call_count: number;
    failure_count: number;
    total_duration_ms: number;
    avg_duration_ms: number;
    min_duration_ms: number;
    max_duration_ms: number;
    p50_duration_ms: number;
    p95_duration_ms: number;
    p99_duration_ms: number;
  }>;
}

export type RestoreResponse = Record<string, unknown>;

/** Wire shape of one run summary in `GET /v1/traces` (ADR-0030 D7). */
export interface TraceRunSummary {
  run_id: string;
  agent_id: string;
  /** Unix epoch seconds. */
  started_at: number;
  /** Unix epoch seconds; `null`/absent when the run is still in flight. */
  ended_at?: number | null;
  prompt_ids: string[];
  experiment_id?: string | null;
  variant_name?: string | null;
  final_status?: string | null;
  judge_score?: number | null;
}

/** Wire shape of `GET /v1/traces`. */
export interface ListTracesResponse {
  runs: TraceRunSummary[];
}

/** One trace event line in the NDJSON body returned by
 *  `GET /v1/traces/:run_id`. Shape is the JSON form of
 *  `awaken_ext_observability::trace_store::TraceEvent`. */
export interface TraceEvent {
  kind: string;
  ts?: number;
  payload?: Record<string, unknown>;
  [key: string]: unknown;
}

/** Result of fetching a trace page — events plus the pagination headers
 *  the server emits (`x-trace-next-offset` / `x-trace-total-events`). */
export interface TracePage {
  events: TraceEvent[];
  total: number;
  next_offset: number | null;
}

/** Wire shape of `GET /v1/agents/:id/permission-preview` (issue #190). */
export interface PermissionPreviewResponse {
  agent_id: string;
  /** `true` when the permission plugin is loaded (`plugin_ids` contains
   *  `"permission"`) AND `active_hook_filter` admits its hooks (filter is
   *  empty, or contains `"permission"`). `false` when the runtime would
   *  not run any permission BeforeInference hooks for this agent — in
   *  which case `effective_tools` equals `candidate_tools` and no rules
   *  are surfaced. */
  permission_plugin_enabled: boolean;
  /** Default behavior when no rule matches. `null` when permission plugin is
   *  not enabled — `effective_tools` are just `candidate_tools` in that case. */
  default_behavior: "allow" | "ask" | "deny" | null;
  /** `allowed_tools ∖ excluded_tools` over the full registered tool set. */
  candidate_tools: string[];
  /** Tools from `candidate_tools` that the BeforeInference hook will
   *  unconditionally strip — only deny rules biting a tool the model would
   *  otherwise see are counted. Deny rules targeting tools already outside
   *  the candidate set (e.g. excluded via `excluded_tools`) are NOT
   *  included so the "stripped before model" summary doesn't overstate.
   *  Empty when permission plugin is disabled. */
  unconditionally_denied: string[];
  /** `candidate_tools ∖ unconditionally_denied`. This is the tool list the
   *  model actually receives. Per-call args-dependent rules can still gate
   *  / Ask / Deny at invocation time — see `args_conditional_rules`. */
  effective_tools: string[];
  /** Rules whose match depends on runtime arguments — informational only. */
  args_conditional_rules: Array<{
    tool: string;
    behavior: "allow" | "ask" | "deny";
    pattern: string;
  }>;
}

// ── Record provenance types (mirrors Rust RecordMeta / RecordSource) ─────────

export type RecordSource = { kind: "builtin"; binary_version: string } | { kind: "user" };

export interface RecordMeta {
  source: RecordSource;
  hidden: boolean;
  user_overrides?: Record<string, unknown> | null;
  created_at: number;
  updated_at: number;
}

export interface ConfigMetaItem {
  id: string;
  meta: RecordMeta;
}

/** Three-state derivation. Pure function — no fetch required. */
export type ConfigSourceState = "builtin" | "customized" | "user";

export function deriveSourceState(meta: RecordMeta): ConfigSourceState {
  // Defensive: handle missing source (e.g. unexpected server response shape).
  if (!meta.source || meta.source.kind === "user") return "user";
  if (
    meta.user_overrides &&
    typeof meta.user_overrides === "object" &&
    Object.keys(meta.user_overrides).length > 0
  ) {
    return "customized";
  }
  return "builtin";
}

export interface ListResponse<T> {
  namespace: string;
  items: T[];
  offset: number;
  limit: number;
}
