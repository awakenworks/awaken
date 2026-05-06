import { BACKEND_URL, ConfigApiError, fetchJson } from "./http";
import type { AgentRuntimeSnapshot } from "./types";

export const agentsApi = {
  /** All-agents runtime stats. Backed by `/v1/agents/runtime-stats`, which
   *  returns `{ "agents": AgentRuntimeSnapshot[] }`. Returns `null` if the
   *  observability registry isn't installed (HTTP 503). */
  agentsRuntimeStats: async (): Promise<{ agents: AgentRuntimeSnapshot[] } | null> => {
    try {
      return await fetchJson<{ agents: AgentRuntimeSnapshot[] }>(
        `${BACKEND_URL}/v1/agents/runtime-stats`,
      );
    } catch (err) {
      if (err instanceof ConfigApiError && err.status === 503) return null;
      throw err;
    }
  },

  patchAgentOverrides: (id: string, patch: Record<string, unknown>) =>
    fetchJson<unknown>(`${BACKEND_URL}/v1/config/agents/${encodeURIComponent(id)}/overrides`, {
      method: "PATCH",
      headers: { "content-type": "application/json" },
      body: JSON.stringify(patch),
    }),

  clearAgentOverrides: (id: string) =>
    fetchJson<unknown>(`${BACKEND_URL}/v1/config/agents/${encodeURIComponent(id)}/overrides`, {
      method: "DELETE",
    }),

  clearAgentOverrideField: (id: string, field: string) =>
    fetchJson<unknown>(
      `${BACKEND_URL}/v1/config/agents/${encodeURIComponent(id)}/overrides/${encodeURIComponent(field)}`,
      { method: "DELETE" },
    ),
};
