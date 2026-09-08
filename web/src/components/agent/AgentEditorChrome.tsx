import { ArrowRight, CornerDownRight, Play, Sparkles } from "@awaken/ui/icons";
import { Tab, TabList, InlineNotice, Button } from "@awaken/ui";
import type { AgentConfig, Capabilities, RuntimeCap, ValidationIssue } from "../../lib/api/types";
import { effectiveAgentToolPermission, selectedAgentToolIds } from "../../lib/agent-toolsets";
import { labelForPath } from "../../lib/config-diff";
import { CONSERVATIVE_DELEGATION_LIMITS } from "../../lib/agent-collaboration";
import { useApp } from "../../lib/app-state";

import type { AuthorStage } from "./agent-editor-navigation";
import { acpWorkingDirectoryIssue } from "../../lib/agent-runtime-capabilities";
import { runtimeStatus } from "../../lib/readiness";
import { webSearchProviderOptions } from "./WebSearchBehaviorEditor";

export const BLANK_AGENT_CONFIG: AgentConfig = {
  id: "",
  name: "",
  model: { mode: "auto" },
  system: "You are a helpful coding agent.",
  metadata: {},
  tools: [],
  mcp_servers: [],
  skills: [],
  multiagent: { type: "coordinator", agents: [{ type: "self" }] },
  delegation_limits: CONSERVATIVE_DELEGATION_LIMITS,
  max_steps: 8,
  plugins: [],
  plugin_config: {},
  context_policy: { kind: "keep_all" },
};

export function buildAgentDraftBody(
  config: AgentConfig,
  id: string,
  permissionPreset: "controlled_modifications" | null,
): AgentConfig & { permission_preset?: "controlled_modifications" } {
  return {
    ...config,
    id,
    ...(permissionPreset ? { permission_preset: permissionPreset } : {}),
  };
}

export function agentNeedsToolBridge(
  config: AgentConfig,
  providerServerTools: readonly string[] = [],
  selectedTools?: readonly string[],
): boolean {
  const providerServer = new Set(providerServerTools);
  const bridgeOwnedTools = selectedTools
    ? selectedTools.some((tool) => !providerServer.has(tool))
      || config.tools.some((tool) => typeof tool === "object" && tool.type !== "agent_toolset_20260401")
    : config.tools.some((tool) => typeof tool !== "string" || !providerServer.has(tool));
  return bridgeOwnedTools
    || config.skills.length > 0
    || config.mcp_servers.length > 0
    || Object.hasOwn(config.plugin_config, "memory");
}

export function agentModelIsRunnable(
  model: AgentConfig["model"],
  readyModels: readonly string[],
  runtimes: readonly RuntimeCap[],
  requiresToolBridge = false,
): boolean {
  if (typeof model === "string") return readyModels.includes(model);
  if ("id" in model) return readyModels.includes(model.id);
  if (model.mode === "backend_default" || model.mode === "backend_exact") {
    if (acpWorkingDirectoryIssue(model.configuration?.working_directory ?? "") !== null) {
      return false;
    }
    return runtimes.some((runtime) => runtime.id === model.backend_ref
      && runtimeStatus(runtime) === "ready"
      && (!requiresToolBridge || runtime.features?.awaken_tool_bridge === "supported"));
  }
  return readyModels.length > 0;
}

export function agentPublicationReadiness(
  config: AgentConfig,
  readyModels: readonly string[],
  capabilities?: Capabilities,
): { modelIsRunnable: boolean; providerApprovalConflict: boolean } {
  const agentToolset = capabilities?.toolsets?.find(
    (toolset) => toolset.type === "agent_toolset_20260401",
  );
  const selectedToolIds = selectedAgentToolIds(config.tools, agentToolset?.members ?? []);
  const providerServerTools = ["web_search", "web_fetch"].filter((toolId) => {
    const plugin = capabilities?.plugins.find((candidate) => candidate.id === toolId);
    const providerId = (config.plugin_config[toolId] as { provider_id?: string } | undefined)
      ?.provider_id;
    return webSearchProviderOptions(plugin?.config_schema)
      .some((provider) => provider.id === providerId && provider.realization === "provider_server");
  });
  return {
    modelIsRunnable: agentModelIsRunnable(
      config.model,
      readyModels,
      capabilities?.runtimes ?? [],
      agentNeedsToolBridge(config, providerServerTools, selectedToolIds),
    ),
    providerApprovalConflict: providerServerTools.some((toolId) =>
      selectedToolIds.includes(toolId)
        && effectiveAgentToolPermission(
          config.tools,
          toolId,
          agentToolset?.default_config.permission_policy,
        ).type === "always_ask"),
  };
}

const STAGES: Array<{
  key: AuthorStage;
  label: string;
  zh: string;
  description: string;
  descriptionZh: string;
}> = [
  { key: "quickstart", label: "Quickstart", zh: "快速开始", description: "Template to first real run", descriptionZh: "从模板到首次真实运行" },
  { key: "build", label: "Build", zh: "构建", description: "Prompt, capabilities and knowledge", descriptionZh: "提示词、能力与知识" },
  { key: "advanced", label: "Advanced", zh: "高级", description: "Orchestration, plugins and source", descriptionZh: "编排、Plugin 与原始配置" },
];

export function AgentEditorStageNavigation({
  changed,
  canTry,
  onTry,
}: {
  changed: (stage: AuthorStage) => boolean;
  canTry: boolean;
  onTry: () => void;
}) {
  const app = useApp();
  return (
    <div className="author-stage-controls">
    <TabList className="author-stage-nav" aria-label={app.t("Authoring stages", "创作阶段")}>
      {STAGES.map((item, index) => (
        <Tab
          key={item.key}
          className="author-stage-button"
          value={item.key}
          aria-label={app.t(item.label, item.zh)}
        >
          <span className="author-stage-index">{index + 1}</span>
          <span>
            <strong>{app.t(item.label, item.zh)}</strong>
            <small>{app.t(item.description, item.descriptionZh)}</small>
          </span>
          {changed(item.key) && <Sparkles className="agent-change-dot" label={app.t("Changed", "已修改")} />}
        </Tab>
      ))}
    </TabList>
      {canTry && <Button variant="ghost" onClick={onTry}><Play /> {app.t("Try draft", "试运行草稿")}</Button>}
    </div>
  );
}

export function AgentValidationIssues({
  issues,
  onOpen,
}: {
  issues: ValidationIssue[];
  onOpen: (path: string) => void;
}) {
  const app = useApp();
  if (issues.length === 0) return null;
  return (
    <InlineNotice tone="warning" className="issue-banner">
      {issues.map((issue, index) => (
        <div className="row" key={`${issue.path}-${index}`} style={{ justifyContent: "space-between" }}>
          <span><strong>{labelForPath(issue.path) || app.t("Config", "配置")}</strong>{" — "}{issue.message}</span>
          <Button variant="ghost" onClick={() => onOpen(issue.path)}>{app.t("Open field", "打开对应字段")} <ArrowRight /></Button>
        </div>
      ))}
    </InlineNotice>
  );
}

export function AttachedAgentContext({ parentId, onBack }: { parentId: string; onBack: () => void }) {
  const app = useApp();
  if (!parentId) return null;
  return (
    <InlineNotice
      tone="info"
      className="attached-agent-context"
      icon={<CornerDownRight />}
      actions={
        <>
          <Button variant="ghost" style={{ marginLeft: "auto" }} onClick={onBack}>
            {app.t("Back to primary Agent", "返回主 Agent")}
          </Button>
        </>
      }
    >
      <span>
        <strong>{app.t(`Specialist for ${parentId}`, `${parentId} 的专属辅助 Agent`)}</strong><br />
        {app.t(
          "This Agent belongs to the primary Agent above. Publish it first, then the parent roster will be updated for review.",
          "此 Agent 归属于上方主 Agent。请先发布它，随后系统会更新主 Agent 清单供你审阅。",
        )}
      </span>
    </InlineNotice>
  );
}
