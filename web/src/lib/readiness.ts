import { useQuery } from "@tanstack/react-query";
import { api, workspaceQuery, ws } from "./api/client";
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

export function deriveReadiness(facts: ReadinessFacts, zh = false): ReadinessItem[] {
  const supplyReady = facts.models > 0 || facts.acp > 0;
  const executionReady = facts.nativeRuntime || facts.acp > 0 || facts.environments > 0;
  const base = `/w/${facts.workspace}`;
  return [
    {
      id: "supply",
      label: facts.managedModels
        ? zh ? "模型" : "Models"
        : zh ? "模型与连接" : "Models and connections",
      detail: supplyReady
        ? facts.managedModels
          ? zh ? `${facts.models} 个云端托管模型可用` : `${facts.models} Cloud-managed models available`
          : zh
            ? `${facts.models} 个可用模型 · ${facts.providerConnections} 个 Provider 连接 · ${facts.acp} 个 ACP`
            : `${facts.models} runnable models · ${facts.providerConnections} Provider connections · ${facts.acp} ACP`
        : facts.managedModels
          ? zh ? "云端模型目录暂时不可用" : "Cloud model catalog is temporarily unavailable"
          : zh ? "连接 Provider，或登录已检测到的 ACP 运行时" : "Connect a Provider or sign in to a detected ACP runtime",
      status: supplyReady ? "ready" : facts.managedModels ? "attention" : "action",
      href: `${base}/models`,
    },
    {
      id: "agent",
      label: zh ? "Agent 发布" : "Agent publication",
      detail: facts.publishedAgents
        ? zh ? `${facts.publishedAgents} 个已发布，可用于 Session` : `${facts.publishedAgents} published and available to Sessions`
        : zh ? "创建、检查并发布一个 Agent" : "Create, validate and publish an Agent",
      status: facts.publishedAgents ? "ready" : "action",
      href: `${base}/agents`,
    },
    {
      id: "execution",
      label: zh ? "运行环境" : "Execution environments",
      detail: executionReady
        ? zh ? `${facts.environments} 个 Environment · ${facts.acp} 个就绪 ACP` : `${facts.environments} Environments · ${facts.acp} ready ACP runtimes`
        : zh ? "当前没有可执行的 Runtime 或 Environment" : "No executable runtime or Environment is available",
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
        ws(workspaceQuery("/v1/config/provider-connections", workspace)),
      ),
    enabled: byokEnabled,
  });
  const credentials = useQuery({
    queryKey: ["credentials", workspace],
    queryFn: () =>
      api.get<CredentialSource[]>(ws(workspaceQuery("/v1/config/credentials", workspace))),
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
  }, app.locale === "zh");

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
