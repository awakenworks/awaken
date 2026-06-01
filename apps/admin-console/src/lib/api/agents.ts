import { BACKEND_URL, ConfigApiError, fetchJson } from "./http";
import type { AgentRuntimeSnapshot, PermissionPreviewResponse } from "./types";

export type AgentsRuntimeStatsResult =
  | { kind: "ok"; agents: AgentRuntimeSnapshot[] }
  | { kind: "route_absent" }
  | { kind: "registry_unavailable" };

export const agentsApi = {
  /** All-agents runtime stats. Backed by `/v1/agents/runtime-stats`, which
   *  returns `{ "agents": AgentRuntimeSnapshot[] }`. 404 means the route is
   *  absent; 503 means the route exists but RuntimeStatsRegistry is unwired.
   *  Auth (401/403) and other 4xx still propagate so the user fixes credentials. */
  agentsRuntimeStats: async (): Promise<AgentsRuntimeStatsResult> => {
    try {
      const payload = await fetchJson<{ agents: AgentRuntimeSnapshot[] }>(
        `${BACKEND_URL}/v1/agents/runtime-stats`,
      );
      return { kind: "ok", agents: payload.agents };
    } catch (err) {
      if (err instanceof ConfigApiError) {
        if (err.status === 404) return { kind: "route_absent" };
        if (err.status === 503) return { kind: "registry_unavailable" };
      }
      throw err;
    }
  },

  patchAgentOverrides: (id: string, patch: Record<string, unknown>) =>
    fetchJson<unknown>(`${BACKEND_URL}/v1/config/agents/${encodeURIComponent(id)}/overrides`, {
      method: "PATCH",
      headers: { "content-type": "application/json" },
      body: JSON.stringify(patch),
    }),

  validateAgentOverrides: (id: string, patch: Record<string, unknown>) =>
    fetchJson<{ ok: boolean; normalized: unknown }>(
      `${BACKEND_URL}/v1/config/agents/${encodeURIComponent(id)}/overrides`,
      {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify(patch),
      },
    ),

  clearAgentOverrides: (id: string) =>
    fetchJson<unknown>(`${BACKEND_URL}/v1/config/agents/${encodeURIComponent(id)}/overrides`, {
      method: "DELETE",
    }),

  clearAgentOverrideField: (id: string, field: string) =>
    fetchJson<unknown>(
      `${BACKEND_URL}/v1/config/agents/${encodeURIComponent(id)}/overrides/${encodeURIComponent(field)}`,
      { method: "DELETE" },
    ),

  /** True effective-tools preview for an agent — runs the permission ruleset
   *  server-side rather than re-implementing it in TS.
   *
   *  Returns `null` only when the server build was compiled WITHOUT the
   *  `permission` feature; the route then returns 503 Service Unavailable
   *  and the UI renders a "preview unavailable" state.
   *
   *  404 means the agent record doesn't exist (stale id, concurrent
   *  delete, typo). Re-throw so the editor can surface a real error
   *  instead of misleading the user with "feature not available". */
  agentPermissionPreview: async (id: string): Promise<PermissionPreviewResponse | null> => {
    try {
      return await fetchJson<PermissionPreviewResponse>(
        `${BACKEND_URL}/v1/agents/${encodeURIComponent(id)}/permission-preview`,
      );
    } catch (err) {
      if (err instanceof ConfigApiError && err.status === 503) return null;
      throw err;
    }
  },
};
