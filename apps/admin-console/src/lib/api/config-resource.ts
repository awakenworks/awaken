import { BACKEND_URL, ConfigApiError, configUrl, fetchJson } from "./http";
import type { ConfigMetaItem, ListResponse, RecordMeta, RestoreResponse } from "./types";

export const configResourceApi = {
  list: <T = unknown>(namespace: string, offset = 0, limit = 100) =>
    fetchJson<ListResponse<T>>(`${configUrl(namespace)}?offset=${offset}&limit=${limit}`),

  get: <T = unknown>(namespace: string, id: string) => fetchJson<T>(configUrl(namespace, id)),

  create: <TBody, TResponse = TBody>(namespace: string, body: TBody) =>
    fetchJson<TResponse>(configUrl(namespace), {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify(body),
    }),

  update: <TBody, TResponse = TBody>(namespace: string, id: string, body: TBody) =>
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

  /** Per-resource metadata list. Returns a plain array.
   *
   *  Some `awaken-server` builds return the bare `Vec<ConfigMetaItem>`
   *  array, others wrap it as `{ items: [...] }` to match the sibling
   *  `list` endpoints. We accept either shape so the UI doesn't crash
   *  with "object is not iterable" when the backend reshapes the
   *  response (which has happened in the wild — see GH-issue/PR). */
  listMeta: async (namespace: string): Promise<ConfigMetaItem[]> => {
    const raw = await fetchJson<unknown>(`${configUrl(namespace)}/meta`);
    return coerceMetaListResponse(raw);
  },
};

/** Defensive parse for `/v1/config/:ns/meta`. Coerces:
 *  - `ConfigMetaItem[]`           → returned as-is
 *  - `{ items: ConfigMetaItem[] }`→ `.items` extracted
 *  - anything else                 → throws `ConfigApiError(502)` so the
 *    query layer renders a typed error state instead of silently
 *    pretending the resource has no entries.
 *  Filters out array members that don't look like `ConfigMetaItem` so a
 *  partial-shape payload doesn't blow up downstream `for...of` loops. */
function coerceMetaListResponse(raw: unknown): ConfigMetaItem[] {
  const candidate = Array.isArray(raw)
    ? raw
    : raw && typeof raw === "object" && Array.isArray((raw as { items?: unknown }).items)
      ? ((raw as { items: unknown[] }).items)
      : null;
  if (candidate === null) {
    throw new ConfigApiError(
      502,
      `unexpected meta response shape: ${describeShape(raw)} — expected ConfigMetaItem[] or { items: ConfigMetaItem[] }`,
    );
  }
  return candidate.filter(
    (entry): entry is ConfigMetaItem =>
      entry !== null &&
      typeof entry === "object" &&
      typeof (entry as { id?: unknown }).id === "string",
  );
}

function describeShape(raw: unknown): string {
  if (raw === null) return "null";
  if (raw === undefined) return "undefined";
  if (Array.isArray(raw)) return "array";
  return typeof raw;
}
