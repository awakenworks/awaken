// Which models the workspace can actually run: an offered model is "ready" only
// when its provider has an active, `can_consume`-compatible credential — so the
// pickers offer only models that will resolve a real executor (config-plane
// truth: catalog offerings × workspace credentials), never a model that would
// fail at run time for want of a key.

import { useQuery } from "@tanstack/react-query";
import { api, ws } from "./api/client";
import type { CredentialSource, ProviderCatalog } from "./api/types";
import { useApp } from "./app-state";

export interface Models {
  /** Model ids whose provider has an active credential — safe to pick. */
  ready: string[];
  /** Every offered model id (ready or not), for context/hints. */
  all: string[];
  /** True while catalog or credentials are still loading. */
  loading: boolean;
}

export function useModels(): Models {
  const workspace = useApp().workspaceId;
  const catalog = useQuery({
    queryKey: ["catalog"],
    queryFn: () => api.get<ProviderCatalog>(ws("/v1/config/catalog")),
  });
  const credentials = useQuery({
    queryKey: ["credentials", workspace],
    queryFn: () => api.get<CredentialSource[]>(ws(`/v1/config/credentials?workspace_id=${workspace}`)),
  });

  const offerings = (catalog.data?.offerings ?? []).filter(
    (offering) => (offering.status ?? "active") === "active",
  );
  const creds = credentials.data ?? [];
  // Native/provider model pickers mirror config-resolver `can_consume`: an
  // unscoped credential serves any provider and a scoped one only its provider.
  // Claude Code setup tokens belong exclusively to `acp:claude`; they are not
  // Anthropic Messages API keys and therefore never make a catalog offering ready.
  const credentialed = (providerId: string) =>
    creds.some(
      (c) =>
        c.status === "active" &&
        c.env_key !== "CLAUDE_CODE_OAUTH_TOKEN" &&
        (c.provider_id == null || c.provider_id === providerId),
    );

  return {
    ready: Array.from(
      new Set(offerings.filter((o) => credentialed(o.provider_id)).map((o) => o.model_id)),
    ),
    all: Array.from(new Set(offerings.map((o) => o.model_id))),
    loading: catalog.isLoading || credentials.isLoading,
  };
}
