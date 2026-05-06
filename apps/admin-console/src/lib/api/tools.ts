import { BACKEND_URL, configUrl, fetchJson } from "./http";
import type { ListResponse, ToolSpec } from "./types";

export const toolsApi = {
  patchToolOverrides: (id: string, patch: { description?: string | null }) =>
    fetchJson<ToolSpec>(`${BACKEND_URL}/v1/config/tools/${encodeURIComponent(id)}/overrides`, {
      method: "PATCH",
      headers: { "content-type": "application/json" },
      body: JSON.stringify(patch),
    }),

  clearToolOverrides: (id: string) =>
    fetchJson<ToolSpec>(`${BACKEND_URL}/v1/config/tools/${encodeURIComponent(id)}/overrides`, {
      method: "DELETE",
    }),

  clearToolOverrideField: (id: string, field: string) =>
    fetchJson<ToolSpec>(
      `${BACKEND_URL}/v1/config/tools/${encodeURIComponent(id)}/overrides/${encodeURIComponent(field)}`,
      { method: "DELETE" },
    ),

  listTools: () => fetchJson<ListResponse<ToolSpec>>(`${BACKEND_URL}/v1/config/tools`),

  getTool: (id: string) => fetchJson<ToolSpec>(configUrl("tools", id)),
};
