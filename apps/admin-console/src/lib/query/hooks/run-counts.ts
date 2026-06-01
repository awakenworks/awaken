import { useQuery } from "@tanstack/react-query";
import { runsApi } from "../../api";
import { DASHBOARD_REFETCH_MS } from "./dashboard";

/** Live count of runs in each non-terminal lifecycle state.
 *
 *  Backed by `GET /v1/runs/summary` — one round-trip for all three
 *  totals (tighter snapshot than three parallel `?status=` queries).
 *
 *  We expose the *reason* a counter is missing so the dashboard can
 *  tell operators "the route doesn't exist (old server)" vs "the run
 *  store is unwired or returning 503" — collapsing both into `null`
 *  hid useful diagnostic information. */
export type RunCounts = {
  running: number;
  waiting: number;
  created: number;
};

export type RunCountsResult =
  | { kind: "ok"; counts: RunCounts }
  /** 404 — the server doesn't expose `/v1/runs/summary`. Either the
   *  build predates the endpoint or the route layer is misconfigured. */
  | { kind: "route_absent" }
  /** 503 — the run store is unwired or transiently unhealthy. */
  | { kind: "store_unavailable" };

export function useRunCountsQuery() {
  return useQuery<RunCountsResult>({
    queryKey: ["run-counts"] as const,
    queryFn: loadRunCounts,
    refetchInterval: DASHBOARD_REFETCH_MS,
  });
}

async function loadRunCounts(): Promise<RunCountsResult> {
  return runsApi.summary();
}
