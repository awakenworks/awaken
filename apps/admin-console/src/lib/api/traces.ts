import { BACKEND_URL, ConfigApiError, fetchJson } from "./http";
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
  /** List recent runs, optionally filtered by agent. Returns `null` when the
   *  server build does not expose trace persistence (HTTP 503 — feature gate
   *  in `awaken-server` controls this), so callers can render a friendly
   *  "not configured" state rather than throwing. */
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
      if (err instanceof ConfigApiError && (err.status === 503 || err.status === 404)) {
        return null;
      }
      throw err;
    }
  },

  /** Fetch one page of trace events for a run, parsing the NDJSON body and
   *  surfacing the server's pagination headers. The server caps a single
   *  page at 1000 events; callers can keep paging while `next_offset !==
   *  null`. */
  getTracePage: async (
    runId: string,
    options: { offset?: number; limit?: number } = {},
  ): Promise<TracePage> => {
    const url = `${BACKEND_URL}/v1/traces/${encodeURIComponent(runId)}${qs({
      offset: options.offset,
      limit: options.limit,
    })}`;
    // Bearer header is applied by the fetch wrapper inside fetchJson — but
    // this endpoint is NDJSON, not JSON, so we mirror its auth handling
    // directly here to keep the NDJSON streaming path simple.
    const token = readStoredAdminToken();
    const init: RequestInit = {
      headers: token ? { authorization: `Bearer ${token}` } : {},
    };
    const response = await fetch(url, init);
    if (response.status === 503 || response.status === 404) {
      // Either trace store not configured (503) or run is unknown (404) —
      // both are "no events" states; let the caller decide UX.
      return { events: [], total: 0, next_offset: null };
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

function readStoredAdminToken(): string | null {
  if (typeof globalThis.localStorage === "undefined") return null;
  const stored = globalThis.localStorage.getItem("awaken.adminToken");
  return stored && stored.trim() ? stored.trim() : null;
}
