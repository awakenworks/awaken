import { useQuery } from "@tanstack/react-query";
import { agentsApi, type AgentsRuntimeStatsResult } from "../../api";
import { DASHBOARD_REFETCH_MS } from "./dashboard";

/** All-agents runtime stats snapshot list. Pulled out of
 *  `useDashboardQuery` so the dashboard composite doesn't reject when
 *  this single subsystem errors — the activity card surfaces the error
 *  inline instead.
 *
 *  Returns a discriminated result so the dashboard can tell "route absent"
 *  (HTTP 404) from "registry not wired" (HTTP 503). Other errors propagate so the consumer can render an
 *  inline error state. */
export function useRuntimeStatsQuery() {
  return useQuery<AgentsRuntimeStatsResult>({
    queryKey: ["agents", "runtime-stats-list"] as const,
    queryFn: agentsApi.agentsRuntimeStats,
    refetchInterval: DASHBOARD_REFETCH_MS,
  });
}
