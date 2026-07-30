import { useQuery } from "@tanstack/react-query";
import { api } from "./api/client";

export const SUITE_NAVIGATION_PATH = "/.well-known/awaken-suite-navigation";

export interface SuiteNavigation {
  readonly hub_url: string | null;
}

export function useSuiteNavigation() {
  return useQuery({
    queryKey: ["suite-navigation"],
    queryFn: () => api.get<SuiteNavigation>(SUITE_NAVIGATION_PATH),
    staleTime: Number.POSITIVE_INFINITY,
    retry: false,
  });
}

/** Optional presentation capability: failure is equivalent to standalone mode. */
export function suiteHubUrl(
  navigation: SuiteNavigation | undefined,
  failed: boolean,
): string | null {
  if (failed) return null;
  return navigation?.hub_url ?? null;
}
