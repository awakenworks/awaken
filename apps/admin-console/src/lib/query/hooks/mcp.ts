import { useQueries, useQuery } from "@tanstack/react-query";
import {
  ConfigApiError,
  mcpApi,
  type McpServerInventoryResponse,
  type McpServerStatusResponse,
} from "../../api";
import { qk } from "../keys";

async function loadMcpStatus(id: string): Promise<McpServerStatusResponse | null> {
  try {
    return await mcpApi.mcpStatus(id);
  } catch (error) {
    if (error instanceof ConfigApiError && error.status === 404) {
      return null;
    }
    throw error;
  }
}

export function useMcpStatusQuery(id: string | undefined) {
  return useQuery<McpServerStatusResponse | null>({
    queryKey: qk.mcp.status(id ?? ""),
    queryFn: () => {
      if (!id) throw new Error("Missing MCP server id");
      return loadMcpStatus(id);
    },
    enabled: Boolean(id),
  });
}

export function useMcpInventoryQuery(id: string | undefined) {
  return useQuery<McpServerInventoryResponse | null>({
    queryKey: qk.mcp.inventory(id ?? ""),
    queryFn: async () => {
      if (!id) throw new Error("Missing MCP server id");
      try {
        return await mcpApi.mcpInventory(id);
      } catch (error) {
        if (error instanceof ConfigApiError && error.status === 404) {
          return null;
        }
        throw error;
      }
    },
    enabled: Boolean(id),
  });
}

export function useMcpStatusQueries(ids: string[]) {
  return useQueries({
    queries: ids.map((id) => ({
      queryKey: qk.mcp.status(id),
      queryFn: () => loadMcpStatus(id),
    })),
  });
}
