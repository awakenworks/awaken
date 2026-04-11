export const BACKEND_URL =
  import.meta.env.VITE_BACKEND_URL ?? "http://127.0.0.1:38080";

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
}

export interface ModelBindingSpec {
  id: string;
  provider_id: string;
  upstream_model: string;
}

export interface ProviderSpec {
  id: string;
  adapter: string;
  api_key?: string;
  base_url?: string;
  timeout_secs?: number;
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
  const response = await fetch(url, init);
  const detail = await readResponseBody(response);

  if (!response.ok) {
    throw new ConfigApiError(response.status, detail);
  }

  if (response.status === 204) {
    return undefined as T;
  }

  return detail as T;
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

  delete: (namespace: string, id: string) =>
    fetchJson<void>(configUrl(namespace, id), { method: "DELETE" }),

  capabilities: async () =>
    normalizeCapabilities(
      await fetchJson<Capabilities>(`${BACKEND_URL}/v1/capabilities`),
    ),
};
