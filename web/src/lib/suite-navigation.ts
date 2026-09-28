// @formal surface requirement=product.admin_console.console_workflows
// @formal reason: Suite navigation combines route, query, and asynchronous state at the UI boundary.
import { useQuery } from "@tanstack/react-query";
import { api, workspaceFromPath } from "./api/client";
import {
  hostedSessionEntry,
  type SuiteNavigation,
} from "./generated/suite-navigation";

export {
  hostedSessionEntry,
  hostedAccessFailure,
  suiteHubUrl,
  suiteProductEntryUrl,
  suiteCloudUrl,
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
