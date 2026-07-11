// Which models the workspace can actually run: an offered model is "ready" only
// when its provider has an active, `can_consume`-compatible credential — so the
// pickers offer only models that will resolve a real executor (config-plane
// truth: catalog offerings × workspace credentials), never a model that would
// fail at run time for want of a key.

import { useQuery } from "@tanstack/react-query";
import { api } from "./api/client";
import type { CredentialSource, ProviderCatalog } from "./api/types";

const WORKSPACE = "wrkspc_default";

export interface Models {
  /** Model ids whose provider has an active credential — safe to pick. */
  ready: string[];
  /** Every offered model id (ready or not), for context/hints. */
  all: string[];
  /** True while catalog or credentials are still loading. */
  loading: boolean;
}

export function useModels(): Models {
  const catalog = useQuery({
    queryKey: ["catalog"],
    queryFn: () => api.get<ProviderCatalog>("/v1/config/catalog"),
  });
  const credentials = useQuery({
    queryKey: ["credentials", WORKSPACE],
    queryFn: () => api.get<CredentialSource[]>(`/v1/config/credentials?workspace_id=${WORKSPACE}`),
  });

  const offerings = catalog.data?.offerings ?? [];
  const creds = credentials.data ?? [];
  // can_consume (ADR-0118): an unscoped credential (provider_id null) serves any
  // provider; a scoped one serves only its provider. Only active credentials count.
  const credentialed = (providerId: string) =>
    creds.some(
      (c) => c.status === "active" && (c.provider_id == null || c.provider_id === providerId),
    );

  return {
    ready: Array.from(
      new Set(offerings.filter((o) => credentialed(o.provider_id)).map((o) => o.model_id)),
    ),
    all: Array.from(new Set(offerings.map((o) => o.model_id))),
    loading: catalog.isLoading || credentials.isLoading,
  };
}
