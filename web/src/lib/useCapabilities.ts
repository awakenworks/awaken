// The capability snapshot hook: host-level tools + installable plugins (with
// config schemas), addressed uniformly through ws() like every management call.
// Runtime readiness is live Worker observation; poll while the UI is open so a
// CLI login/logout becomes visible without restarting Awaken.

import { useQuery } from "@tanstack/react-query";
import { api, ws } from "./api/client";
import type { Capabilities } from "./api/types";

export function useCapabilities() {
  return useQuery({
    queryKey: ["capabilities"],
    queryFn: () => api.get<Capabilities>(ws("/v1/capabilities")),
    staleTime: 2_000,
    refetchInterval: 5_000,
  });
}
