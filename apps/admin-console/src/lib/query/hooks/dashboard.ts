import { useQuery } from "@tanstack/react-query";
import {
  capabilitiesApi,
  capabilitiesFromResult,
  configResourceApi,
  ConfigApiError,
  type AgentSpec,
  type Capabilities,
  type McpServerRecord,
  type ModelSpec,
  type ProviderRecord,
  type SystemInfo,
} from "../../api";
import { auditApi } from "../../api/audit";
import type { AuditPage } from "../../audit-log";
import { TIME_RANGE_SECONDS, type TimeRange } from "../../../components/ui/time-range-switcher";
import { qk } from "../keys";
import { loadOptionalSystemInfo } from "../system-info";

/** How often the dashboard re-fetches its composite query. The same
 *  interval is surfaced to the UI ("refreshes every {{seconds}}s") so
 *  there is one source of truth. */
export const DASHBOARD_REFETCH_MS = 30_000;

/** Per-slot degradation flags. Set when a sub-query soft-degraded
 *  (typically a 5xx that returned an empty list). UI uses this to
 *  distinguish "configured empty" from "list endpoint failed". */
export interface DegradedSlots {
  mcpServers?: boolean;
  providers?: boolean;
  models?: boolean;
  agents?: boolean;
}

export type DashboardData = {
  capabilities: Capabilities;
  mcpServers: McpServerRecord[];
  providers: ProviderRecord[];
  models: ModelSpec[];
  agents: AgentSpec[];
  auditPage: AuditPage | null;
  auditDisabled: boolean;
  systemInfo: SystemInfo | null;
  degraded: DegradedSlots;
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
      // 5xx on any individual list endpoint soft-degrades to an empty
      // page rather than crashing the dashboard with a red error box.
      // Auth (401/403) and 4xx still propagate — the user needs to
      // re-auth, not stare at half-blank cards.
      const [capabilitiesResult, mcp, providers, models, agents, audit, systemInfo] =
        await Promise.all([
          capabilitiesApi.capabilities(),
          tolerantList(configResourceApi.list<McpServerRecord>("mcp-servers")),
          tolerantList(configResourceApi.list<ProviderRecord>("providers")),
          tolerantList(configResourceApi.list<ModelSpec>("models")),
          tolerantList(configResourceApi.list<AgentSpec>("agents")),
          auditPromise,
          loadOptionalSystemInfo(),
        ]);
      const degraded: DegradedSlots = {};
      if (mcp.degraded) degraded.mcpServers = true;
      if (providers.degraded) degraded.providers = true;
      if (models.degraded) degraded.models = true;
      if (agents.degraded) degraded.agents = true;
      if (capabilitiesResult.kind !== "ok") {
        throw new ConfigApiError(
          capabilitiesResult.kind === "route_absent" ? 404 : 503,
          capabilitiesResult.kind === "route_absent"
            ? "capabilities route is not exposed"
            : (capabilitiesResult.message ?? "capabilities store is unavailable"),
        );
      }
      const capabilities = capabilitiesFromResult(capabilitiesResult);
      if (!capabilities) {
        throw new ConfigApiError(500, "capabilities response was empty");
      }
      return {
        capabilities,
        mcpServers: mcp.items,
        providers: providers.items,
        models: models.items,
        agents: agents.items,
        auditPage: audit.page,
        auditDisabled: audit.disabled,
        systemInfo,
        degraded,
      };
    },
    refetchInterval: DASHBOARD_REFETCH_MS,
  });
}

/** Wrap a `configResourceApi.list` call so a transient 5xx returns an
 *  empty list flagged as `degraded` instead of crashing the dashboard.
 *  The flag is surfaced to the UI so a card can show "list unavailable"
 *  rather than misleading the operator with "no items configured". */
async function tolerantList<T>(
  p: Promise<{ items: T[]; total?: number; has_more?: boolean }>,
): Promise<{ items: T[]; total?: number; has_more?: boolean; degraded?: boolean }> {
  try {
    return await p;
  } catch (err) {
    if (err instanceof ConfigApiError && err.status >= 500) {
      console.warn("[dashboard] sub-query failed, soft-degrading to empty:", err);
      return { items: [], total: 0, has_more: false, degraded: true };
    }
    throw err;
  }
}
