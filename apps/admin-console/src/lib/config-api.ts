import {
  hasUnauthorizedHandler,
  requestUnauthorizedRetry,
} from "./auth-interceptor";
import { type AuditPage, type AuditQuery, buildAuditQueryString } from "./audit-log";

export const BACKEND_URL =
  import.meta.env.VITE_BACKEND_URL ?? "http://127.0.0.1:38080";

export const ADMIN_TOKEN_STORAGE_KEY = "awaken.adminToken";

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

export type RecordSource =
  | { kind: "builtin"; binary_version: string }
  | { kind: "user" };

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

export class ConfigApiError extends Error {
  readonly status: number;
  readonly detail: unknown;

  constructor(status: number, detail: unknown) {
    super(extractErrorMessage(status, detail));
    this.name = "ConfigApiError";
    this.status = status;
    this.detail = detail;
  }
}

function extractErrorMessage(status: number, detail: unknown): string {
  if (typeof detail === "string" && detail.trim().length > 0) {
    return detail;
  }

  if (
    detail &&
    typeof detail === "object" &&
    "error" in detail &&
    typeof detail.error === "string"
  ) {
    return detail.error;
  }

  return `Request failed with status ${status}`;
}

async function readResponseBody(response: Response): Promise<unknown> {
  const text = await response.text();
  if (!text) {
    return null;
  }

  try {
    return JSON.parse(text) as unknown;
  } catch {
    return text;
  }
}

async function fetchJson<T>(url: string, init?: RequestInit): Promise<T> {
  let response = await fetch(url, withAdminAuth(init));
  if (response.status === 401 && hasUnauthorizedHandler()) {
    const refreshed = await requestUnauthorizedRetry();
    if (refreshed && refreshed.trim().length > 0) {
      response = await fetch(url, withAdminAuth(init, refreshed.trim()));
    }
  }

  const detail = await readResponseBody(response);

  if (!response.ok) {
    throw new ConfigApiError(response.status, detail);
  }

  if (response.status === 204) {
    return undefined as T;
  }

  return detail as T;
}

function adminBearerToken(override?: string): string | undefined {
  if (typeof override === "string" && override.trim().length > 0) {
    return override.trim();
  }

  const envToken = import.meta.env.VITE_ADMIN_BEARER_TOKEN;
  if (typeof envToken === "string" && envToken.trim().length > 0) {
    return envToken.trim();
  }

  if (typeof globalThis.localStorage === "undefined") {
    return undefined;
  }
  const stored = globalThis.localStorage.getItem(ADMIN_TOKEN_STORAGE_KEY);
  return stored?.trim() || undefined;
}

function withAdminAuth(init?: RequestInit, override?: string): RequestInit | undefined {
  const token = adminBearerToken(override);
  if (!token) {
    return init;
  }

  const headers = new Headers(init?.headers);
  headers.set("authorization", `Bearer ${token}`);
  return {
    ...init,
    headers,
  };
}

function normalizeCapabilities(capabilities: Capabilities): Capabilities {
  return {
    ...capabilities,
    skills: (capabilities.skills ?? []).map((skill) => {
      const allowedTools = skill.allowed_tools ?? [];
      const argumentsList = skill.arguments ?? [];
      const paths = skill.paths ?? [];
      return {
        ...skill,
        allowed_tools: allowedTools,
        arguments: argumentsList,
        paths,
      };
    }),
  };
}

function configUrl(namespace: string, id?: string): string {
  const base = `${BACKEND_URL}/v1/config/${namespace}`;
  return id ? `${base}/${encodeURIComponent(id)}` : base;
}

export function agentPreviewRunUrl(): string {
  return `${BACKEND_URL}/v1/ai-sdk/agent-previews/runs`;
}

export const configApi = {
  list: <T = unknown>(namespace: string, offset = 0, limit = 100) =>
    fetchJson<ListResponse<T>>(
      `${configUrl(namespace)}?offset=${offset}&limit=${limit}`,
    ),

  get: <T = unknown>(namespace: string, id: string) =>
    fetchJson<T>(configUrl(namespace, id)),

  create: <TBody, TResponse = TBody>(namespace: string, body: TBody) =>
    fetchJson<TResponse>(configUrl(namespace), {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify(body),
    }),

  update: <TBody, TResponse = TBody>(
    namespace: string,
    id: string,
    body: TBody,
  ) =>
    fetchJson<TResponse>(configUrl(namespace, id), {
      method: "PUT",
      headers: { "content-type": "application/json" },
      body: JSON.stringify(body),
    }),

  delete: (namespace: string, id: string, options?: { force?: boolean }) => {
    const url = options?.force
      ? `${configUrl(namespace, id)}?force=true`
      : configUrl(namespace, id);
    return fetchJson<void>(url, { method: "DELETE" });
  },

  capabilities: async () =>
    normalizeCapabilities(
      await fetchJson<Capabilities>(`${BACKEND_URL}/v1/capabilities`),
    ),

  testProvider: (id: string) =>
    fetchJson<{ ok: boolean; latency_ms: number; error?: string }>(
      `${BACKEND_URL}/v1/providers/${encodeURIComponent(id)}/test`,
      { method: "POST" },
    ),

  mcpStatus: (id: string) =>
    fetchJson<McpServerStatusResponse>(
      `${BACKEND_URL}/v1/mcp-servers/${encodeURIComponent(id)}/status`,
    ),

  /** Server identity + uptime + which optional subsystems are wired. */
  systemInfo: () =>
    fetchJson<SystemInfo>(`${BACKEND_URL}/v1/system/info`),

  /** Dry-run validate. Backend runs the same prepare+validate path as
   *  create/update but does NOT persist or apply. Returns the normalized
   *  payload (timestamps, derived fields) on success; throws ConfigApiError
   *  with the same shape as a real save on failure. */
  validateConfig: <TBody>(namespace: string, body: TBody, opts?: { id?: string }) => {
    const qs = opts?.id ? `?id=${encodeURIComponent(opts.id)}` : "";
    return fetchJson<{ ok: boolean; normalized: unknown }>(
      `${BACKEND_URL}/v1/config/${encodeURIComponent(namespace)}/validate${qs}`,
      {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify(body),
      },
    );
  },

  /** All-agents runtime stats. Backed by `/v1/agents/runtime-stats`, which
   *  returns `{ "agents": AgentRuntimeSnapshot[] }`. Returns `null` if the
   *  observability registry isn't installed (HTTP 503). */
  agentsRuntimeStats: async (): Promise<{ agents: AgentRuntimeSnapshot[] } | null> => {
    try {
      return await fetchJson<{ agents: AgentRuntimeSnapshot[] }>(
        `${BACKEND_URL}/v1/agents/runtime-stats`,
      );
    } catch (err) {
      if (err instanceof ConfigApiError && err.status === 503) return null;
      throw err;
    }
  },

  mcpRestart: (id: string) =>
    fetchJson<void>(`${BACKEND_URL}/v1/mcp-servers/${encodeURIComponent(id)}/restart`, {
      method: "POST",
    }),

  auditLog: async (query: AuditQuery): Promise<AuditPage> => {
    const qs = buildAuditQueryString(query).toString();
    const url = `${BACKEND_URL}/v1/audit-log${qs ? `?${qs}` : ""}`;
    try {
      return await fetchJson<AuditPage>(url);
    } catch (err) {
      if (err instanceof ConfigApiError && err.status === 503) {
        throw new ConfigApiError(503, "audit log not configured");
      }
      throw err;
    }
  },

  restoreConfig: (namespace: string, id: string, version: string) =>
    fetchJson<RestoreResponse>(
      `${BACKEND_URL}/v1/config/${encodeURIComponent(namespace)}/${encodeURIComponent(id)}/restore`,
      {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ version }),
      },
    ),

  getMeta: (namespace: string, id: string) =>
    fetchJson<RecordMeta>(`${configUrl(namespace, id)}/meta`),

  listMeta: (namespace: string) =>
    fetchJson<ConfigMetaItem[]>(`${configUrl(namespace)}/meta`),

  patchAgentOverrides: (id: string, patch: Record<string, unknown>) =>
    fetchJson<unknown>(
      `${BACKEND_URL}/v1/config/agents/${encodeURIComponent(id)}/overrides`,
      {
        method: "PATCH",
        headers: { "content-type": "application/json" },
        body: JSON.stringify(patch),
      },
    ),

  clearAgentOverrides: (id: string) =>
    fetchJson<unknown>(
      `${BACKEND_URL}/v1/config/agents/${encodeURIComponent(id)}/overrides`,
      { method: "DELETE" },
    ),

  clearAgentOverrideField: (id: string, field: string) =>
    fetchJson<unknown>(
      `${BACKEND_URL}/v1/config/agents/${encodeURIComponent(id)}/overrides/${encodeURIComponent(field)}`,
      { method: "DELETE" },
    ),

  patchToolOverrides: (id: string, patch: { description?: string | null }) =>
    fetchJson<ToolSpec>(
      `${BACKEND_URL}/v1/config/tools/${encodeURIComponent(id)}/overrides`,
      {
        method: "PATCH",
        headers: { "content-type": "application/json" },
        body: JSON.stringify(patch),
      },
    ),

  clearToolOverrides: (id: string) =>
    fetchJson<ToolSpec>(
      `${BACKEND_URL}/v1/config/tools/${encodeURIComponent(id)}/overrides`,
      { method: "DELETE" },
    ),

  clearToolOverrideField: (id: string, field: string) =>
    fetchJson<ToolSpec>(
      `${BACKEND_URL}/v1/config/tools/${encodeURIComponent(id)}/overrides/${encodeURIComponent(field)}`,
      { method: "DELETE" },
    ),

  listTools: () =>
    fetchJson<ListResponse<ToolSpec>>(`${BACKEND_URL}/v1/config/tools`),

  getTool: (id: string) =>
    fetchJson<ToolSpec>(configUrl("tools", id)),
};
