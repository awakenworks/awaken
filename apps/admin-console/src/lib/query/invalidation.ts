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
    if (namespace === "agents") {
      // The permission-preview API reads the persisted agent spec to
      // compute effective tools, so any agent save / reset / restore
      // must invalidate it. Without this, the editor would keep showing
      // the previous effective list for `staleTime` after a permission
      // section edit was saved.
      void queryClient.invalidateQueries({ queryKey: qk.agent.permissionPreview(id) });
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
