import { BACKEND_URL, ConfigApiError, fetchJson } from "./http";
import type { AgentRuntimeSnapshot, PermissionPreviewResponse } from "./types";

export const agentsApi = {
  /** All-agents runtime stats. Backed by `/v1/agents/runtime-stats`, which
   *  returns `{ "agents": AgentRuntimeSnapshot[] }`. Returns `null` if the
   *  observability registry isn't installed (HTTP 503) OR if the server
   *  predates the endpoint (HTTP 404 — older deploys / gradual rollout).
   *  Both surfaces as the same "feature unavailable" notice; auth (401/403)
   *  and other 4xx still propagate so the user fixes credentials. */
  agentsRuntimeStats: async (): Promise<{ agents: AgentRuntimeSnapshot[] } | null> => {
    try {
      return await fetchJson<{ agents: AgentRuntimeSnapshot[] }>(
        `${BACKEND_URL}/v1/agents/runtime-stats`,
      );
    } catch (err) {
      if (err instanceof ConfigApiError && (err.status === 503 || err.status === 404)) {
        return null;
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
