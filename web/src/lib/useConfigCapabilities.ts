import { useQuery } from "@tanstack/react-query";
import type { ConfigCapabilitiesView } from "./api/types";
import { api, ws } from "./api/client";
import { useApp } from "./app-state";

/** One React Query resource owns the deployment's model-supply capability
 * projection. Consumers share its key, request and cache rather than inventing
 * component-local Cloud/local detection. */
export function useConfigCapabilities() {
  const workspace = useApp().workspaceId;
  return useQuery({
    queryKey: ["config-capabilities", workspace],
    queryFn: () => api.get<ConfigCapabilitiesView>(ws("/v1/config/capabilities")),
    staleTime: Infinity,
  });
}
