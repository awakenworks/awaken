import { useQuery } from "@tanstack/react-query";
import { capabilitiesApi, type CapabilitiesResult } from "../../api";
import { qk } from "../keys";

interface CapabilitiesQueryOptions {
  enabled?: boolean;
}

export function useCapabilitiesQuery(options: CapabilitiesQueryOptions = {}) {
  return useQuery<CapabilitiesResult>({
    queryKey: qk.capabilities(),
    queryFn: capabilitiesApi.capabilities,
    enabled: options.enabled ?? true,
  });
}
