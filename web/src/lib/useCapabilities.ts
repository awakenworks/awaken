// The capability snapshot hook: host-level tools + installable plugins (with
// config schemas), addressed uniformly through ws() like every management call.
// Cached long — capabilities are deployment-level and change only on deploy.

import { useQuery } from "@tanstack/react-query";
import { api, ws } from "./api/client";
import type { Capabilities } from "./api/types";

export function useCapabilities() {
  return useQuery({
    queryKey: ["capabilities"],
    queryFn: () => api.get<Capabilities>(ws("/v1/capabilities")),
    staleTime: 5 * 60_000,
  });
}
