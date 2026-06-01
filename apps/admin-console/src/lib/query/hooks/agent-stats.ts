import { useQuery } from "@tanstack/react-query";
import { agentsApi, type AgentsRuntimeStatsResult } from "../../api";
import { fetchAgentRuntimeStats, type AgentRuntimeStatsResult } from "../../agent-stats";
import { qk } from "../keys";

export function useAgentRuntimeStatsQuery(agentId: string | undefined, window: string) {
  return useQuery<AgentRuntimeStatsResult>({
    queryKey: qk.agent.runtimeStats(agentId ?? "", window),
    queryFn: () => {
      if (!agentId) {
        throw new Error("Missing agent id");
      }
      return fetchAgentRuntimeStats(agentId, window ? { window } : undefined);
    },
    enabled: Boolean(agentId),
  });
}

export function useAgentsRuntimeStatsQuery() {
  return useQuery<AgentsRuntimeStatsResult>({
    queryKey: qk.agent.runtimeStatsList(),
    queryFn: agentsApi.agentsRuntimeStats,
  });
}
