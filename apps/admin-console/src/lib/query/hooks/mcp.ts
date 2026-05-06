import { useQueries, useQuery } from "@tanstack/react-query";
import { mcpApi, type McpServerStatusResponse } from "../../api";
import { qk } from "../keys";

export function useMcpStatusQuery(id: string | undefined) {
  return useQuery<McpServerStatusResponse | null>({
    queryKey: qk.mcp.status(id ?? ""),
    queryFn: () => {
      if (!id) throw new Error("Missing MCP server id");
      return mcpApi.mcpStatus(id).catch(() => null);
    },
    enabled: Boolean(id),
  });
}

export function useMcpStatusQueries(ids: string[]) {
  return useQueries({
    queries: ids.map((id) => ({
      queryKey: qk.mcp.status(id),
      queryFn: () => mcpApi.mcpStatus(id).catch(() => null),
    })),
  });
}
