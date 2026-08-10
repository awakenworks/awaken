import { useQuery } from "@tanstack/react-query";
import { api } from "./api/client";
import type { SuiteNavigation } from "./generated/suite-navigation";

export {
  hostedSessionEntry,
  type HostedSessionEntry,
  type SuiteNavigation,
} from "./generated/suite-navigation";

export const SUITE_NAVIGATION_PATH = "/.well-known/awaken-suite-navigation";

export const suiteNavigationQuery = {
  queryKey: ["suite-navigation"] as const,
  queryFn: () => api.get<SuiteNavigation>(SUITE_NAVIGATION_PATH),
  staleTime: Number.POSITIVE_INFINITY,
  retry: false,
};

export function useSuiteNavigation() {
  return useQuery(suiteNavigationQuery);
}

/** Optional presentation capability: failure is equivalent to standalone mode. */
export function suiteHubUrl(
  navigation: SuiteNavigation | undefined,
  failed: boolean,
): string | null {
  if (failed) return null;
  return navigation?.hub_url ?? null;
}
