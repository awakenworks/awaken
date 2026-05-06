import type { QueryClient } from "@tanstack/react-query";
import { qk } from "./keys";

const NAV_HEALTH_NAMESPACES = new Set(["agents", "mcp-servers", "providers"]);
const DASHBOARD_NAMESPACES = new Set(["agents", "mcp-servers", "providers", "models"]);
const CAPABILITIES_NAMESPACES = new Set(["agents", "mcp-servers", "providers", "models", "tools"]);

export function invalidateConfigMutation(queryClient: QueryClient, namespace: string, id?: string) {
  void queryClient.invalidateQueries({ queryKey: qk.config.listRoot(namespace) });
  void queryClient.invalidateQueries({ queryKey: qk.config.listMeta(namespace) });

  if (id) {
    void queryClient.invalidateQueries({ queryKey: qk.config.get(namespace, id) });
    void queryClient.invalidateQueries({ queryKey: qk.config.meta(namespace, id) });
    if (namespace === "mcp-servers") {
      void queryClient.invalidateQueries({ queryKey: qk.mcp.status(id) });
    }
  }

  if (NAV_HEALTH_NAMESPACES.has(namespace)) {
    void queryClient.invalidateQueries({ queryKey: qk.navHealth() });
  }
  if (DASHBOARD_NAMESPACES.has(namespace)) {
    void queryClient.invalidateQueries({ queryKey: qk.dashboardRoot() });
  }
  if (CAPABILITIES_NAMESPACES.has(namespace)) {
    void queryClient.invalidateQueries({ queryKey: qk.capabilities() });
  }
}

export function removeConfigResourceQueries(
  queryClient: QueryClient,
  namespace: string,
  id: string,
) {
  queryClient.removeQueries({ queryKey: qk.config.get(namespace, id), exact: true });
  queryClient.removeQueries({ queryKey: qk.config.meta(namespace, id), exact: true });
  if (namespace === "mcp-servers") {
    queryClient.removeQueries({ queryKey: qk.mcp.status(id), exact: true });
  }
}
