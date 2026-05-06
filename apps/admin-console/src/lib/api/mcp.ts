import { BACKEND_URL, fetchJson } from "./http";
import type { McpServerStatusResponse } from "./types";

export const mcpApi = {
  mcpStatus: (id: string) =>
    fetchJson<McpServerStatusResponse>(
      `${BACKEND_URL}/v1/mcp-servers/${encodeURIComponent(id)}/status`,
    ),

  mcpRestart: (id: string) =>
    fetchJson<void>(`${BACKEND_URL}/v1/mcp-servers/${encodeURIComponent(id)}/restart`, {
      method: "POST",
    }),
};
