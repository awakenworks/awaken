import { useQuery } from "@tanstack/react-query";
import {
  capabilitiesApi,
  configResourceApi,
  ConfigApiError,
  type AgentSpec,
  type Capabilities,
  type McpServerRecord,
  type ModelBindingSpec,
  type ProviderRecord,
  type SystemInfo,
} from "../../api";
import { auditApi } from "../../api/audit";
import type { AuditPage } from "../../audit-log";
import { TIME_RANGE_SECONDS, type TimeRange } from "../../../components/ui/time-range-switcher";
import { qk } from "../keys";
import { loadOptionalSystemInfo } from "../system-info";

export type DashboardData = {
  capabilities: Capabilities;
  mcpServers: McpServerRecord[];
  providers: ProviderRecord[];
  models: ModelBindingSpec[];
  agents: AgentSpec[];
  auditPage: AuditPage | null;
  auditDisabled: boolean;
  systemInfo: SystemInfo | null;
};

export function useDashboardQuery(range: TimeRange) {
  return useQuery<DashboardData>({
    queryKey: qk.dashboard(range),
    queryFn: async () => {
      const sinceMs = Date.now() - TIME_RANGE_SECONDS[range] * 1000;
      const since = new Date(sinceMs).toISOString();
      const auditPromise = auditApi
        .auditLog({ limit: 50, since })
        .then((page) => ({ page, disabled: false }))
        .catch((err) => {
          if (err instanceof ConfigApiError && err.status === 503) {
            return { page: null, disabled: true };
          }
          throw err;
        });
      const [capabilities, mcp, providers, models, agents, audit, systemInfo] = await Promise.all([
        capabilitiesApi.capabilities(),
        configResourceApi.list<McpServerRecord>("mcp-servers"),
        configResourceApi.list<ProviderRecord>("providers"),
        configResourceApi.list<ModelBindingSpec>("models"),
        configResourceApi.list<AgentSpec>("agents"),
        auditPromise,
        loadOptionalSystemInfo(),
      ]);
      return {
        capabilities,
        mcpServers: mcp.items,
        providers: providers.items,
        models: models.items,
        agents: agents.items,
        auditPage: audit.page,
        auditDisabled: audit.disabled,
        systemInfo,
      };
    },
    refetchInterval: 30_000,
  });
}
