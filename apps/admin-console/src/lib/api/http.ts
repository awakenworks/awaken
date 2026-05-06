import { hasUnauthorizedHandler, requestUnauthorizedRetry } from "../auth-interceptor";

export const BACKEND_URL = import.meta.env.VITE_BACKEND_URL ?? "http://127.0.0.1:38080";

export const ADMIN_TOKEN_STORAGE_KEY = "awaken.adminToken";

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

export async function fetchJson<T>(url: string, init?: RequestInit): Promise<T> {
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

  if (typeof globalThis.localStorage === "undefined") {
    return devEnvAdminBearerToken();
  }
  const stored = globalThis.localStorage.getItem(ADMIN_TOKEN_STORAGE_KEY);
  const storedToken = stored?.trim();
  if (storedToken) {
    return storedToken;
  }
  return devEnvAdminBearerToken();
}

function devEnvAdminBearerToken(): string | undefined {
  if (import.meta.env.PROD) {
    return undefined;
  }
  const envToken = import.meta.env.VITE_ADMIN_BEARER_TOKEN;
  return typeof envToken === "string" ? envToken.trim() || undefined : undefined;
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

export function configUrl(namespace: string, id?: string): string {
  const base = `${BACKEND_URL}/v1/config/${namespace}`;
  return id ? `${base}/${encodeURIComponent(id)}` : base;
}

export function agentPreviewRunUrl(): string {
  return `${BACKEND_URL}/v1/ai-sdk/agent-previews/runs`;
}
