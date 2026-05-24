import { useQuery } from "@tanstack/react-query";
import { agentsApi, type AgentRuntimeSnapshot } from "../../api";
import { DASHBOARD_REFETCH_MS } from "./dashboard";

/** All-agents runtime stats snapshot list. Pulled out of
 *  `useDashboardQuery` so the dashboard composite doesn't reject when
 *  this single subsystem errors — the activity card surfaces the error
 *  inline instead.
 *
 *  Returns `null` when the endpoint reports "feature not configured"
 *  (HTTP 503) or "endpoint absent" (HTTP 404 — older deploy / partial
 *  rollout). Other errors propagate so the consumer can render an
 *  inline error state. */
export function useRuntimeStatsQuery() {
  return useQuery<AgentRuntimeSnapshot[] | null>({
    queryKey: ["agents", "runtime-stats-list"] as const,
    queryFn: async () => {
      const res = await agentsApi.agentsRuntimeStats();
      return res?.agents ?? null;
    },
    refetchInterval: DASHBOARD_REFETCH_MS,
  });
}
