// The ONLY module that touches fetch (enforced by web/scripts/lint.mjs).
// Injects the IAM bearer, and maps both error dialects the host speaks:
// RFC 9457 problem+json (admin plane) and the managed error envelope
// { type: "error", error: { type, message } } (sessions/vaults).

const TOKEN_KEY = "awaken.console.token";

export function getToken(): string {
  return localStorage.getItem(TOKEN_KEY) ?? "";
}
export function setToken(token: string): void {
  if (token) localStorage.setItem(TOKEN_KEY, token);
  else localStorage.removeItem(TOKEN_KEY);
}

// ---- workspace scope seam (ADR-0048 path addressing / ADR-0051 tenancy) ----
// Tenancy resolves to one opaque scope. With NO workspace set, scoped calls go to
// the flat `/v1/…` surface (DEFAULT_SCOPE — single-tenant, Option A). Setting a
// workspace routes them through `/v1/workspaces/{ws}/…`, which the host rewrites
// back to flat `/v1/…` and stamps `{ws}` as the edge scope (multi-tenant, Option B).
const WS_KEY = "awaken.console.workspace";
export function getWorkspace(): string {
  return localStorage.getItem(WS_KEY) ?? "";
}
export function setWorkspace(workspace: string): void {
  if (workspace) localStorage.setItem(WS_KEY, workspace);
  else localStorage.removeItem(WS_KEY);
}
/** Scope a flat `/v1/...` path to the active workspace. No workspace → unchanged
 * (default scope); otherwise `/v1/foo` → `/v1/workspaces/{ws}/foo`. This is the
 * one seam every tenant-scoped call routes through, so enabling multi-tenant
 * addressing is a single setWorkspace() away. */
export function ws(path: string): string {
  const w = getWorkspace();
  if (!w) return path;
  return path.replace(/^\/v1\//, `/v1/workspaces/${w}/`);
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
  let requestId: string | undefined;
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

async function request<T>(method: string, path: string, body?: unknown): Promise<T> {
  const headers: Record<string, string> = {};
  const token = getToken();
  if (token) headers.authorization = `Bearer ${token}`;
  if (body !== undefined) headers["content-type"] = "application/json";
  const res = await fetch(path, {
    method,
    headers,
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  if (!res.ok) throw await toError(res);
  if (res.status === 204) return undefined as T;
  return (await res.json()) as T;
}

/** Multipart upload (the Files API is the one endpoint that takes bytes, not JSON).
 * FormData sets its own multipart content-type + boundary, so we must NOT set it. */
async function upload<T>(path: string, file: File, fields?: Record<string, string>): Promise<T> {
  const headers: Record<string, string> = {};
  const token = getToken();
  if (token) headers.authorization = `Bearer ${token}`;
  const form = new FormData();
  form.append("file", file);
  for (const [k, v] of Object.entries(fields ?? {})) form.append(k, v);
  const res = await fetch(path, { method: "POST", headers, body: form });
  if (!res.ok) throw await toError(res);
  return (await res.json()) as T;
}

/** Download a file's bytes and save them under `filename`. The content endpoint carries
 * the bearer like any GET, so it goes through the auth path — not a bare `<a href>` that
 * would omit the token. Symmetric with `upload`. */
async function download(path: string, filename: string): Promise<void> {
  const headers: Record<string, string> = {};
  const token = getToken();
  if (token) headers.authorization = `Bearer ${token}`;
  const res = await fetch(path, { headers });
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
  post: <T>(path: string, body?: unknown) => request<T>("POST", path, body),
  put: <T>(path: string, body?: unknown) => request<T>("PUT", path, body),
  del: <T>(path: string) => request<T>("DELETE", path),
  upload,
  download,
};

/** URL for EventSource consumers (sessions SSE; that face carries no bearer). */
export function streamUrl(path: string): string {
  return path;
}
