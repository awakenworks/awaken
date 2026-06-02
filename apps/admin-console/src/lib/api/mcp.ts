import { BACKEND_URL, fetchJson } from "./http";
import type { McpServerInventoryResponse, McpServerStatusResponse } from "./types";

export const mcpApi = {
  mcpStatus: (id: string) =>
    fetchJson<McpServerStatusResponse>(
      `${BACKEND_URL}/v1/mcp-servers/${encodeURIComponent(id)}/status`,
    ),

  mcpInventory: (id: string) =>
    fetchJson<McpServerInventoryResponse>(
      `${BACKEND_URL}/v1/mcp-servers/${encodeURIComponent(id)}/inventory`,
    ),

  mcpRestart: (id: string) =>
    fetchJson<void>(`${BACKEND_URL}/v1/mcp-servers/${encodeURIComponent(id)}/restart`, {
      method: "POST",
    }),
};
