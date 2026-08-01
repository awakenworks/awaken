import { useQuery } from "@tanstack/react-query";
import { api, ws } from "./api/client";
import type {
  AgentConfigList,
  Capabilities,
  CredentialSource,
  Environment,
  Page,
  ProviderConnectionSummary,
  RuntimeCap,
} from "./api/types";
import { useApp } from "./app-state";
import { useModels } from "./useModels";
import { useConfigCapabilities } from "./useConfigCapabilities";

export type ReadinessStatus = "ready" | "action" | "attention";

export interface ReadinessItem {
  id: "supply" | "agent" | "execution";
  label: string;
  detail: string;
  status: ReadinessStatus;
  href: string;
}

export interface ReadinessFacts {
  workspace: string;
  models: number;
  providerConnections: number;
  acp: number;
  publishedAgents: number;
  environments: number;
  nativeRuntime: boolean;
  managedModels: boolean;
}

export function deriveReadiness(facts: ReadinessFacts): ReadinessItem[] {
  const supplyReady = facts.models > 0 || facts.acp > 0;
  const executionReady = facts.nativeRuntime || facts.acp > 0 || facts.environments > 0;
  const base = `/w/${facts.workspace}`;
  return [
    {
      id: "supply",
      label: facts.managedModels ? "Models" : "AI supply",
      detail: supplyReady
        ? facts.managedModels
          ? `${facts.models} Cloud-managed models available`
          : `${facts.models} runnable models · ${facts.providerConnections} provider connections · ${facts.acp} ACP`
        : facts.managedModels
          ? "Cloud model catalog is temporarily unavailable"
          : "Connect a Provider or sign in to a detected ACP runtime",
      status: supplyReady ? "ready" : facts.managedModels ? "attention" : "action",
      href: `${base}/models`,
    },
    {
      id: "agent",
      label: "Agent",
      detail: facts.publishedAgents
        ? `${facts.publishedAgents} published and available to Sessions`
        : "Create, validate and publish an Agent",
      status: facts.publishedAgents ? "ready" : "action",
      href: `${base}/agents`,
    },
    {
      id: "execution",
      label: "Execution",
      detail: executionReady
        ? `${facts.environments} environments · ${facts.acp} ready ACP runtimes`
        : "No executable runtime or environment is available",
      status: executionReady ? "ready" : "attention",
      href: `${base}/environments`,
    },
  ];
}

export function runtimeStatus(runtime: RuntimeCap): "ready" | "login_required" | "not_detected" {
  if (runtime.kind === "native") return "ready";
  if (!runtime.local?.detected) return "not_detected";
  return runtime.local.login_state === "available" ? "ready" : "login_required";
}

/** One read projection over existing authorities. It persists nothing and makes
 * no selection: each remediation routes to the aggregate that owns the fact. */
export function useWorkspaceReadiness() {
  const app = useApp();
  const workspace = app.workspaceId;
  const models = useModels();
  const configCapabilities = useConfigCapabilities();
  const byokEnabled = configCapabilities.data?.models.byok_enabled === true;
  const connections = useQuery({
    queryKey: ["provider-connections", workspace],
    queryFn: () =>
      api.get<ProviderConnectionSummary[]>(
        ws(`/v1/config/provider-connections?workspace_id=${workspace}`),
      ),
    enabled: byokEnabled,
  });
  const credentials = useQuery({
    queryKey: ["credentials", workspace],
    queryFn: () =>
      api.get<CredentialSource[]>(ws(`/v1/config/credentials?workspace_id=${workspace}`)),
    enabled: byokEnabled,
  });
  const capabilities = useQuery({
    queryKey: ["capabilities", workspace],
    queryFn: () => api.get<Capabilities>(ws("/v1/capabilities")),
  });
  const agents = useQuery({
    queryKey: ["config-agents", workspace],
    queryFn: () => api.get<AgentConfigList>(ws("/v1/config/agents")),
  });
  const environments = useQuery({
    queryKey: ["environments", workspace],
    queryFn: () => api.get<Page<Environment>>(ws("/v1/environments")),
  });

  const readyConnections = (connections.data ?? []).filter((item) => item.status === "ready");
  const readyAcp = (capabilities.data?.runtimes ?? []).filter(
    (runtime) => runtime.kind === "acp" && runtimeStatus(runtime) === "ready",
  );
  const publishedAgents = (agents.data?.data ?? []).filter((agent) => agent.published);
  const activeEnvironments = (environments.data?.data ?? []).filter((env) => !env.archived_at);
  const nativeReady = (capabilities.data?.runtimes ?? []).some(
    (runtime) => runtime.kind === "native" && runtimeStatus(runtime) === "ready",
  );
  const loading = [
    models.loading,
    connections.isLoading,
    credentials.isLoading,
    capabilities.isLoading,
    agents.isLoading,
    environments.isLoading,
  ].some(Boolean);
  const items = deriveReadiness({
    workspace,
    models: models.ready.length,
    providerConnections: readyConnections.length,
    acp: readyAcp.length,
    publishedAgents: publishedAgents.length,
    environments: activeEnvironments.length,
    nativeRuntime: nativeReady,
    managedModels: configCapabilities.data?.models.byok_enabled === false,
  });

  return {
    items,
    loading,
    ready: !loading && items.every((item) => item.status === "ready"),
    counts: {
      models: models.ready.length,
      credentials: (credentials.data ?? []).filter((item) => item.status === "active").length,
      agents: publishedAgents.length,
      environments: activeEnvironments.length,
      acp: readyAcp.length,
    },
  };
}
