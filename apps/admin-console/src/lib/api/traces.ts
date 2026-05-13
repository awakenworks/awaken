import { BACKEND_URL, ConfigApiError, fetchJson, fetchWithAdminAuth } from "./http";
import type { ListTracesResponse, TraceEvent, TracePage } from "./types";

/** Build query string from a sparse object — undefined values are dropped. */
function qs(params: Record<string, string | number | undefined>): string {
  const entries = Object.entries(params).filter(
    ([, value]) => value !== undefined && value !== "",
  );
  if (entries.length === 0) return "";
  const search = new URLSearchParams();
  for (const [key, value] of entries) {
    search.set(key, String(value));
  }
  return `?${search.toString()}`;
}

export const tracesApi = {
  /** List recent runs, optionally filtered by agent. Returns `null` ONLY
   *  when the server build does not expose trace persistence (HTTP 503 —
   *  feature gate in `awaken-server` controls this), so callers can render
   *  a friendly "not configured" state. A 404 (unknown agent id) is a real
   *  error and is re-thrown so the UI surfaces it as such instead of
   *  conflating it with the "feature disabled" state — matches the
   *  `agentPermissionPreview` / `getTracePage` policy. */
  listAgentTraces: async (
    agentId: string,
    options: { limit?: number; since?: string } = {},
  ): Promise<ListTracesResponse | null> => {
    try {
      return await fetchJson<ListTracesResponse>(
        `${BACKEND_URL}/v1/traces${qs({
          agent_id: agentId,
          limit: options.limit,
          since: options.since,
        })}`,
      );
    } catch (err) {
      if (err instanceof ConfigApiError && err.status === 503) {
        return null;
      }
      throw err;
    }
  },

  /** Fetch one page of trace events for a run, parsing the NDJSON body and
   *  surfacing the server's pagination headers. The server caps a single
   *  page at 1000 events; callers can keep paging while `next_offset !==
   *  null`.
   *
   *  Returns `null` only when the trace store is not configured on this
   *  server build (503) — the UI then renders a "trace persistence not
   *  enabled" state.
   *
   *  404 means the run id is unknown (deleted, never persisted, typo)
   *  and is surfaced as a thrown `ConfigApiError` so the caller can
   *  render a real error rather than showing "no events" for a missing
   *  run. */
  getTracePage: async (
    runId: string,
    options: { offset?: number; limit?: number } = {},
  ): Promise<TracePage | null> => {
    const url = `${BACKEND_URL}/v1/traces/${encodeURIComponent(runId)}${qs({
      offset: options.offset,
      limit: options.limit,
    })}`;
    // R11 #2 — route the NDJSON request through `fetchWithAdminAuth`
    // so it picks up the same admin-bearer resolution (localStorage +
    // dev-env fallback) AND 401-refresh retry as JSON endpoints.
    // Previously this path read only `localStorage` and would fail
    // silently in dev environments that rely on `VITE_ADMIN_BEARER_TOKEN`
    // or against a server that requires a token refresh.
    const response = await fetchWithAdminAuth(url);
    if (response.status === 503) {
      // Trace store not configured on this server build — surface as
      // a "feature unavailable" state, not as an empty event list.
      return null;
    }
    if (!response.ok) {
      const text = await response.text();
      throw new ConfigApiError(response.status, text);
    }
    const total = Number.parseInt(
      response.headers.get("x-trace-total-events") ?? "0",
      10,
    );
    const nextOffsetHeader = response.headers.get("x-trace-next-offset");
    const nextOffset =
      nextOffsetHeader === null ? null : Number.parseInt(nextOffsetHeader, 10);
    const body = await response.text();
    const events: TraceEvent[] = body
      .split("\n")
      .filter((line) => line.trim().length > 0)
      .map((line) => JSON.parse(line) as TraceEvent);
    return {
      events,
      total: Number.isFinite(total) ? total : events.length,
      next_offset: Number.isFinite(nextOffset as number) ? (nextOffset as number) : null,
    };
  },
};

