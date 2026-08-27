// The ONLY module that touches fetch (enforced by web/scripts/lint.mjs).
// Injects the IAM bearer, and maps both error dialects the host speaks:
// RFC 9457 problem+json (admin plane) and the managed error envelope
// { type: "error", error: { type, message } } (sessions/vaults).

const CLOUD_SESSION_TOKEN_KEY = "awaken.product.session-bearer";

export const API_BETAS = {
  managed: "managed-agents-2026-04-01",
  memory: "agent-memory-2026-07-22",
  skills: "skills-2025-10-02",
  dreaming: "dreaming-2026-04-21",
} as const;

/**
 * Select the exact protocol opt-in for one API family. Memory and Skills are
 * intentionally exclusive families: the server rejects a Managed beta on
 * Memory routes, so this must be path-aware rather than a global default.
 */
export function betaForPath(path: string): string | undefined {
  const pathname = path.split(/[?#]/, 1)[0];
  const family = pathname.match(/^\/v1\/(?:workspaces\/[^/]+\/)?([^/]+)/)?.[1];
  if (family === "memory_stores") return API_BETAS.memory;
  if (family === "skills") return API_BETAS.skills;
  if (family === "dreams") {
    return `${API_BETAS.managed},${API_BETAS.dreaming}`;
  }
  if (["sessions", "agents", "environments", "deployments", "deployment_runs", "vaults"].includes(family ?? "")) {
    return API_BETAS.managed;
  }
  return undefined;
}

function requestHeaders(path: string, extra?: Record<string, string>): Record<string, string> {
  const headers: Record<string, string> = { ...(extra ?? {}) };
  const beta = betaForPath(path);
  if (beta) headers["anthropic-beta"] = beta;
  const token = getToken();
  if (token) headers.authorization = `Bearer ${token}`;
  return headers;
}

export function getToken(): string {
  try {
    return globalThis.sessionStorage?.getItem(CLOUD_SESSION_TOKEN_KEY)
      ?? "";
  } catch {
    return "";
  }
}

export function clearProductSessionBearer(): void {
  try {
    globalThis.sessionStorage?.removeItem(CLOUD_SESSION_TOKEN_KEY);
  } catch {
    // The redirect still fails closed when browser storage is unavailable.
  }
}

// ---- workspace scope seam (ADR-0048 path addressing / ADR-0051 tenancy) ----
// Tenancy resolves to one opaque scope. With no route or server-resolved
// workspace, scoped calls use the flat `/v1/…` surface (standalone default
// scope). A hosted `/w/{workspace}` route is the sole browser-selected IAM
// coordinate; the stored workspace below remains a presentation preference.
const WS_KEY = "awaken.console.workspace";
let resolvedWorkspaceId = "";
export function getWorkspace(): string {
  return localStorage.getItem(WS_KEY) ?? "";
}
export function setWorkspace(workspace: string): void {
  if (workspace) localStorage.setItem(WS_KEY, workspace);
  else localStorage.removeItem(WS_KEY);
}

export function workspaceFromPath(pathname: string): string {
  const encoded = pathname.match(/^\/w\/([^/]+)/)?.[1];
  if (!encoded) return "";
  try {
    return decodeURIComponent(encoded);
  } catch {
    return "";
  }
}

function routeWorkspace(): string {
  return workspaceFromPath(globalThis.location?.pathname ?? "");
}

/** Pure decision seam: the route-selected workspace wins; the authenticated
 * server context resolves the presentation-only `default` label; only a real
 * non-default display id may be used as a final explicit fallback. */
export function workspaceIdForRequest(
  displayWorkspace: string,
  pathWorkspace: string,
  resolvedWorkspace: string,
): string | undefined {
  return pathWorkspace || resolvedWorkspace || (displayWorkspace && displayWorkspace !== "default" ? displayWorkspace : undefined);
}

export function setResolvedWorkspace(workspace: string): void {
  resolvedWorkspaceId = workspace;
}

export function requestWorkspaceId(displayWorkspace: string): string | undefined {
  return workspaceIdForRequest(displayWorkspace, routeWorkspace(), resolvedWorkspaceId);
}

export function workspaceQuery(path: string, displayWorkspace: string): string {
  const workspace = requestWorkspaceId(displayWorkspace);
  if (!workspace) return path;
  return `${path}${path.includes("?") ? "&" : "?"}workspace_id=${encodeURIComponent(workspace)}`;
}

export function workspaceFields(displayWorkspace: string): { workspace_id?: string } {
  const workspace_id = requestWorkspaceId(displayWorkspace);
  return workspace_id ? { workspace_id } : {};
}
/** Scope a flat `/v1/...` path to the route or authenticated server context.
 * No trusted coordinate means standalone default scope; localStorage never
 * selects the authorization target. */
export function ws(path: string): string {
  const w = routeWorkspace() || resolvedWorkspaceId;
  if (!w) return path;
  return path.replace(/^\/v1\//, `/v1/workspaces/${encodeURIComponent(w)}/`);
}

export class ApiClientError extends Error {
  status: number;
  code: string;
  requestId?: string;
  constructor(status: number, code: string, message: string, requestId?: string) {
    super(message);
    this.status = status;
    this.code = code;
    this.requestId = requestId;
  }
}

/** A 404/405 from a face that is not mounted — used for capability gating. */
export function isAbsent(err: unknown): boolean {
  return err instanceof ApiClientError && (err.status === 404 || err.status === 405);
}

async function toError(res: Response): Promise<ApiClientError> {
  let code = `http_${res.status}`;
  let message = res.statusText || `HTTP ${res.status}`;
  let requestId = res.headers.get("x-request-id")
    ?? res.headers.get("x-correlation-id")
    ?? undefined;
  try {
    const body: unknown = await res.json();
    if (typeof body === "object" && body !== null) {
      const b = body as Record<string, unknown>;
      if (typeof b.code === "string") {
        // RFC 9457 problem details.
        code = b.code;
        message = [b.title, b.detail].filter((x) => typeof x === "string").join(": ") || message;
        if (typeof b.request_id === "string") requestId = b.request_id;
      } else if (b.type === "error" && typeof b.error === "object" && b.error !== null) {
        // Managed error envelope.
        const e = b.error as Record<string, unknown>;
        if (typeof e.type === "string") code = e.type;
        if (typeof e.message === "string") message = e.message;
      }
    }
  } catch {
    /* non-JSON error body — keep the HTTP defaults */
  }
  return new ApiClientError(res.status, code, message, requestId);
}

async function request<T>(method: string, path: string, body?: unknown, extraHeaders?: Record<string, string>): Promise<T> {
  const headers = requestHeaders(path, extraHeaders);
  if (body !== undefined) headers["content-type"] = "application/json";
  const res = await fetch(path, {
    method,
    headers,
    credentials: "same-origin",
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  if (!res.ok) throw await toError(res);
  if (res.status === 204) return undefined as T;
  return (await res.json()) as T;
}

/** Multipart upload (the Files API is the one endpoint that takes bytes, not JSON).
 * FormData sets its own multipart content-type + boundary, so we must NOT set it. */
async function upload<T>(path: string, file: File, fields?: Record<string, string>): Promise<T> {
  const headers = requestHeaders(path);
  const form = new FormData();
  form.append("file", file);
  for (const [k, v] of Object.entries(fields ?? {})) form.append(k, v);
  const res = await fetch(path, { method: "POST", headers, body: form, credentials: "same-origin" });
  if (!res.ok) throw await toError(res);
  return (await res.json()) as T;
}

export interface MultipartFile {
  readonly path: string;
  readonly blob: Blob;
}

/** Upload a complete path-preserving bundle. Skill directory import and the
 * browser editor both use this exact seam, so neither invents a second wire
 * representation for authored content. */
async function uploadMany<T>(
  path: string,
  files: readonly MultipartFile[],
  fields?: Record<string, string>,
  headers?: Record<string, string>,
): Promise<T> {
  const resolvedHeaders = requestHeaders(path, headers);
  const form = new FormData();
  for (const file of files) form.append("files", file.blob, file.path);
  for (const [key, value] of Object.entries(fields ?? {})) form.append(key, value);
  const response = await fetch(path, {
    method: "POST",
    headers: resolvedHeaders,
    body: form,
    credentials: "same-origin",
  });
  if (!response.ok) throw await toError(response);
  return (await response.json()) as T;
}

async function bytes(path: string): Promise<ArrayBuffer> {
  const headers = requestHeaders(path);
  const response = await fetch(path, { headers, credentials: "same-origin" });
  if (!response.ok) throw await toError(response);
  return response.arrayBuffer();
}

/** Download a file's bytes and save them under `filename`. The content endpoint carries
 * the bearer like any GET, so it goes through the auth path — not a bare `<a href>` that
 * would omit the token. Symmetric with `upload`. */
async function download(path: string, filename: string): Promise<void> {
  const headers = requestHeaders(path);
  const res = await fetch(path, { headers, credentials: "same-origin" });
  if (!res.ok) throw await toError(res);
  const blob = await res.blob();
  const url = URL.createObjectURL(blob);
  const a = document.createElement("a");
  a.href = url;
  a.download = filename;
  a.click();
  URL.revokeObjectURL(url);
}

export const api = {
  get: <T>(path: string) => request<T>("GET", path),
  post: <T>(path: string, body?: unknown, headers?: Record<string, string>) => request<T>("POST", path, body, headers),
  put: <T>(path: string, body?: unknown) => request<T>("PUT", path, body),
  del: <T>(path: string) => request<T>("DELETE", path),
  upload,
  uploadMany,
  bytes,
  download,
};

/**
 * One browser operation's retry identity. An exact payload keeps its key across
 * timeout/retry; editing the payload starts a new operation. The server remains
 * the durable idempotency authority—this class stores no Session receipt.
 */
export class IdempotencyScope {
  private current?: { fingerprint: string; key: string };

  constructor(private readonly prefix: string) {}

  headersFor(payload: unknown): Record<string, string> {
    const fingerprint = JSON.stringify(payload);
    if (!this.current || this.current.fingerprint !== fingerprint) {
      this.current = {
        fingerprint,
        key: `${this.prefix}-${globalThis.crypto.randomUUID()}`,
      };
    }
    return { "idempotency-key": this.current.key };
  }

  /** Close an operation after its success is known. A later identical user
   * intent is a new operation and must not resolve to the old response. */
  complete(): void {
    this.current = undefined;
  }
}

export async function resolveWorkspaceContext(): Promise<string> {
  const selected = routeWorkspace();
  const context = await api.get<{ workspace_id: string }>(ws("/v1/config/workspace-context"));
  if (selected && context.workspace_id !== selected) {
    throw new ApiClientError(
      403,
      "workspace_context_mismatch",
      "The authenticated Workspace does not match this route.",
    );
  }
  setResolvedWorkspace(context.workspace_id);
  return context.workspace_id;
}

export interface ApplicationAccessTokenRequest {
  authority_id: string;
  application_scope: string;
  actor_key?: string;
  protocols: Array<"ai-sdk" | "ag-ui">;
  operations: Array<"thread.run" | "thread.messages.read">;
  thread_bindings: Array<{
    external_thread_id: string;
    managed_session_id: string;
  }>;
  expires_in_seconds?: number;
}

export interface IssuedApplicationAccessToken {
  id: string;
  object: "application_access_token";
  token_type: "Bearer";
  access_token: string;
  expires_at: string;
  application_scope: string;
  protocols: Array<"ai-sdk" | "ag-ui">;
  operations: Array<"thread.run" | "thread.messages.read">;
}

/** Exchange the console's management credential for a short-lived application
 * credential. The returned secret stays in the caller's memory; this module does
 * not persist it in localStorage. */
export function issueApplicationAccessToken(
  request: ApplicationAccessTokenRequest,
): Promise<IssuedApplicationAccessToken> {
  return api.post<IssuedApplicationAccessToken>(ws("/v1/application-access-tokens"), request);
}

/** URL for EventSource consumers (sessions SSE; that face carries no bearer). */
export function streamUrl(path: string): string {
  return path;
}
