export interface AgentSpec {
  id: string;
  model_id: string;
  system_prompt: string;
  max_rounds?: number;
  max_continuation_retries?: number;
  plugin_ids?: string[];
  sections?: Record<string, unknown>;
  allowed_tools?: string[] | null;
  excluded_tools?: string[] | null;
  delegates?: string[];
  reasoning_effort?: string | number | null;
  created_at?: number;
  updated_at?: number;
}

export interface ToolSpec {
  id: string;
  name: string;
  description: string;
  category?: string | null;
  parameters_schema?: unknown;
}

export interface ModelBindingSpec {
  id: string;
  provider_id: string;
  upstream_model: string;
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
  config_schemas: Array<{ key: string; schema: Record<string, unknown> }>;
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

export interface Capabilities {
  agents: string[];
  tools: ToolInfo[];
  plugins: PluginInfo[];
  skills: SkillInfo[];
  models: ModelBindingSpec[];
  providers: Array<{ id: string }>;
  supported_adapters?: string[];
  namespaces: Array<{
    namespace: string;
    schema: Record<string, unknown>;
  }>;
}

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
}

/** Wire-format mirror of `GET /v1/system/info` response. */
export interface SystemInfo {
  version: string;
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
