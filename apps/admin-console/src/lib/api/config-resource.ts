import { BACKEND_URL, configUrl, fetchJson } from "./http";
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

  listMeta: (namespace: string) => fetchJson<ConfigMetaItem[]>(`${configUrl(namespace)}/meta`),
};
