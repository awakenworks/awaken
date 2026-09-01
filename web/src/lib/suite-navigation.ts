import { useQuery } from "@tanstack/react-query";
import { api, workspaceFromPath } from "./api/client";
import {
  hostedSessionEntry,
  type SuiteNavigation,
} from "./generated/suite-navigation";

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

/**
 * Pure presentation intent. Cloud remains responsible for authenticating the
 * account and resolving an authorized launch; the product never learns a
 * sibling origin, Workspace, placement, or readiness coordinate.
 */
export function suiteProductEntryUrl(hubUrl: string, product: "awaken" | "flow"): string {
  const entry = new URL(hubUrl);
  entry.searchParams.delete("continue");
  entry.searchParams.set("product", product);
  return entry.href;
}

/** Build one Cloud-owned account destination from the projected deployment hub. */
export function suiteCloudUrl(hubUrl: string, path: "/products" | "/usage-billing" | "/settings" | "/logout"): string {
  const destination = new URL(hubUrl);
  destination.pathname = path;
  destination.search = "";
  destination.hash = "";
  return destination.href;
}

export type HostedBootstrapDecision =
  | { kind: "standalone" }
  | { kind: "verify" }
  | { kind: "redirect"; url: string };

export function hostedBootstrapDecision(
  navigation: SuiteNavigation,
  sessionBearer: string,
  currentUrl: string,
  pathname: string,
): HostedBootstrapDecision {
  if (!navigation.hub_url) return { kind: "standalone" };
  const exactRoute = workspaceFromPath(pathname);
  const entry = hostedSessionEntry(
    navigation,
    exactRoute ? sessionBearer : "",
    currentUrl,
  );
  return entry.kind === "redirect" ? entry : { kind: "verify" };
}

export function hostedAccessFailure(status: number): "restart" | "denied" | "unavailable" {
  if (status === 401) return "restart";
  if (status === 403) return "denied";
  return "unavailable";
}
