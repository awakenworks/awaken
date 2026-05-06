import { useQuery } from "@tanstack/react-query";
import { capabilitiesApi, type Capabilities } from "../../api";
import { qk } from "../keys";

interface CapabilitiesQueryOptions {
  enabled?: boolean;
}

export function useCapabilitiesQuery(options: CapabilitiesQueryOptions = {}) {
  return useQuery<Capabilities>({
    queryKey: qk.capabilities(),
    queryFn: capabilitiesApi.capabilities,
    enabled: options.enabled ?? true,
  });
}
