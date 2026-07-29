// Which models the workspace can actually run. The server owns the Catalog ×
// Credential readiness join; pickers consume that projection without rebuilding
// provider compatibility rules in the browser.

import { useQuery } from "@tanstack/react-query";
import { api, ws } from "./api/client";
import type { ExecutableModelOption } from "./api/types";
import { useApp } from "./app-state";

export interface Models {
  /** Model ids whose provider has an active credential — safe to pick. */
  ready: string[];
  /** Every offered model id (ready or not), for context/hints. */
  all: string[];
  /** True while the executable-model projection is loading. */
  loading: boolean;
}

export function useModels(): Models {
  const workspace = useApp().workspaceId;
  const models = useQuery({
    queryKey: ["executable-models", workspace],
    queryFn: () => api.get<ExecutableModelOption[]>(
      ws(`/v1/config/executable-models?workspace_id=${workspace}`),
    ),
  });
  const options = models.data ?? [];

  return {
    ready: Array.from(
      new Set(options.filter((option) => option.readiness === "ready").map((option) => option.model_id)),
    ),
    all: Array.from(new Set(options.map((option) => option.model_id))),
    loading: models.isLoading,
  };
}
