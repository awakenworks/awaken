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
  allowed_tools?: string[];
  excluded_tools?: string[];
  delegates?: string[];
  reasoning_effort?: string | number | null;
  created_at?: number;
  updated_at?: number;
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
    fetchJson<{
      connected: boolean;
      last_error?: string | null;
      tools: Array<{ name: string; description?: string | null }>;
    }>(`${BACKEND_URL}/v1/mcp-servers/${encodeURIComponent(id)}/status`),

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
};
