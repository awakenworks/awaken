import { useQuery } from "@tanstack/react-query";
import { api } from "./api/client";

export const SUITE_NAVIGATION_PATH = "/.well-known/awaken-suite-navigation";

export interface SuiteNavigation {
  readonly hub_url: string | null;
}

export type HostedSessionEntry =
  | { readonly kind: "continue" }
  | { readonly kind: "redirect"; readonly url: string };

/**
 * Decide only whether an unauthenticated hosted browser must return to the
 * suite hub. The hub remains opaque; IAM and Cloud own all login coordinates.
 */
export function hostedSessionEntry(
  navigation: SuiteNavigation,
  sessionBearer: string,
): HostedSessionEntry {
  return navigation.hub_url && !sessionBearer
    ? { kind: "redirect", url: navigation.hub_url }
    : { kind: "continue" };
}

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
