import { BACKEND_URL, ConfigApiError, fetchJson } from "./http";

/** Coarse run lifecycle status — mirrors Rust `RunStatus`. */
export type RunStatus = "created" | "running" | "waiting" | "done";

/** Page envelope returned by `GET /v1/runs`. We only model what the
 *  dashboard needs (`total`); callers wanting full `RunRecord` rows can
 *  extend this later. The backend clamps `limit` to `[1, 200]`. */
export interface ListRunsPage {
  items: unknown[];
  total: number;
  has_more: boolean;
}

export interface ListRunsParams {
  status?: RunStatus;
  offset?: number;
  limit?: number;
}

function buildRunsQuery(params: ListRunsParams): string {
  const sp = new URLSearchParams();
  if (params.status) sp.set("status", params.status);
  if (params.offset !== undefined) sp.set("offset", String(params.offset));
  if (params.limit !== undefined) sp.set("limit", String(params.limit));
  const qs = sp.toString();
  return qs ? `?${qs}` : "";
}

/** Counters returned by admin-authenticated `GET /v1/runs/summary`. One round-trip
 *  replaces 3 parallel `?status=` queries; see the Rust handler
 *  doc for the snapshot-consistency caveat. */
export interface RunsSummary {
  running: number;
  waiting: number;
  created: number;
}

export type RunsSummaryResult =
  | { kind: "ok"; counts: RunsSummary }
  | { kind: "route_absent" }
  | { kind: "store_unavailable" };

export const runsApi = {
  list: (params: ListRunsParams = {}): Promise<ListRunsPage> =>
    fetchJson<ListRunsPage>(`${BACKEND_URL}/v1/runs${buildRunsQuery(params)}`),

  summary: async (): Promise<RunsSummaryResult> => {
    try {
      const counts = await fetchJson<RunsSummary>(`${BACKEND_URL}/v1/runs/summary`);
      return { kind: "ok", counts };
    } catch (err) {
      if (err instanceof ConfigApiError) {
        if (err.status === 404) return { kind: "route_absent" };
        if (err.status === 503) return { kind: "store_unavailable" };
      }
      throw err;
    }
  },
};
