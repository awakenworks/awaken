import type {
  AgentConfig,
  AgentToolsetCap,
  ContextPolicy,
  CredentialSource,
  InputBinding,
  ManagedToolsetCap,
  PluginCap,
  ResourceInputDefaultMounts,
  RuntimeCap,
} from "../../lib/api/types";
import { useEffect, useState } from "react";
import {
  controlledModificationMemberNames,
  selectedAgentToolIds,
  withSelectedAgentTools,
} from "../../lib/agent-toolsets";
import type { JsonSchema } from "../ui";
import {
  Button,
  Card,
  CheckPicker,
  Segmented,
  TextAreaField,
  TextField,
} from "../ui";
import { useApp } from "../../lib/app-state";
import type { BuilderSection } from "./agent-editor-navigation";
import AgentIntegrationsEditor from "./AgentIntegrationsEditor";
import AgentAdvancedToolsEditor from "./AgentAdvancedToolsEditor";
import AgentModelSelectionEditor from "./AgentModelSelectionEditor";
import AgentModelRuntimeControls from "./AgentModelRuntimeControls";
import { isAcpModelSelection } from "../../lib/agent-model-selection";
import BehaviorCard from "./BehaviorCard";
import ResourcesTab from "./ResourcesTab";
import ToolOverridesEditor from "./ToolOverridesEditor";

export default function AgentBuilder({
  section,
  config,
  isNew,
  readyModels,
  allModels,
  runtimes,
  tools,
  toolsets,
  agentToolset,
  plugins,
  credentials,
  resources,
  resourceInputDefaults,
  resourcesError,
  changed,
  onSectionChange,
  onPatch,
  onApplyControlledModifications,
  onManageModels,
  onResourcesChange,
  onRetryResources,
  onValidityChange,
}: {
  section: BuilderSection;
  config: AgentConfig;
  isNew: boolean;
  readyModels: string[];
  allModels: string[];
  runtimes: RuntimeCap[];
  tools: Array<{ id: string; description: string }>;
  toolsets: ManagedToolsetCap[];
  agentToolset?: AgentToolsetCap;
  plugins: PluginCap[];
  credentials: CredentialSource[];
  resources: InputBinding[];
  resourceInputDefaults?: ResourceInputDefaultMounts;
  resourcesError?: Error;
  changed: (path: string) => boolean;
  onSectionChange: (section: BuilderSection) => void;
  onPatch: (patch: Partial<AgentConfig>) => void;
  onApplyControlledModifications: () => void;
  onManageModels: () => void;
  onResourcesChange: (inputs: InputBinding[]) => void;
  onRetryResources: () => void;
  onValidityChange: (valid: boolean) => void;
}) {
  const app = useApp();
  const agentToolsetMembers = agentToolset?.members ?? [];
  const [toolsValid, setToolsValid] = useState(true);
  const [integrationsValid, setIntegrationsValid] = useState(true);
  useEffect(() => {
    onValidityChange(toolsValid && integrationsValid);
  }, [integrationsValid, onValidityChange, toolsValid]);
  const selectedToolIds = selectedAgentToolIds(config.tools, agentToolsetMembers);
  const acp = isAcpModelSelection(config.model);
  const backgroundConfig = (config.plugin_config.background_task as { tools?: string[] } | undefined) ?? {};
  const backgroundTools = backgroundConfig.tools ?? [];
  const toolPolicyOverrides = [
    ...(config.tool_overrides ?? []),
    ...backgroundTools
      .filter((tool) => !(config.tool_overrides ?? []).some((entry) => entry.target === tool))
      .map((target) => ({ target })),
  ];
  const controlledPresetAvailable = agentToolsetMembers.some(
    (member) => member.controlled_modification,
  );
  const controlledMemberNames = controlledModificationMemberNames(agentToolsetMembers);
  const controlledMemberDescription = controlledMemberNames.length > 0
    ? controlledMemberNames.join(", ")
    : app.t("advertised controlled-modification tools", "已公布的受控修改工具");
  const sections: Array<{ key: BuilderSection; label: string; zh: string }> = [
    { key: "instructions", label: "Instructions", zh: "提示词" },
    { key: "tools", label: "Tools & permissions", zh: "工具与权限" },
    { key: "integrations", label: "Skills & MCP", zh: "Skills 与 MCP" },
    { key: "knowledge", label: "Memory & resources", zh: "Memory 与资源" },
  ];
  const behavior = (id: string) => plugins.find((plugin) => plugin.id === id);
  const renderBehavior = (id: string) => {
    const plugin = behavior(id);
    if (!plugin) return null;
    return (
      <BehaviorCard
        id={plugin.id}
        schema={plugin.config_schema as JsonSchema | undefined}
        enabled={config.plugins.includes(plugin.id)}
        config={(config.plugin_config[plugin.id] as Record<string, unknown>) ?? {}}
        changed={changed(`plugin_config.${plugin.id}`)}
        credentials={credentials}
        onToggle={(enabled) => {
          if (enabled) {
            onPatch({ plugins: Array.from(new Set([...config.plugins, plugin.id])) });
          } else {
            const { [plugin.id]: _removed, ...rest } = config.plugin_config;
            onPatch({
              plugins: config.plugins.filter((candidate) => candidate !== plugin.id),
              plugin_config: rest,
            });
          }
        }}
        onConfig={(value) => onPatch({
          plugin_config: { ...config.plugin_config, [plugin.id]: value },
        })}
      />
    );
  };

  return (
    <div className="editor-section">
      <div className="subsection-tabs" role="tablist" aria-label={app.t("Build sections", "构建分区")}>
        {sections.map((item) => (
          <Button
            key={item.key}
            role="tab"
            aria-selected={section === item.key}
            variant={section === item.key ? "primary" : "ghost"}
            onClick={() => onSectionChange(item.key)}
          >
            {app.t(item.label, item.zh)}
          </Button>
        ))}
      </div>

      {section === "instructions" && (
        <Card>
          <div className="row">
            <div className={`field${changed("id") ? " agent-change-highlight" : ""}`} style={{ flex: 1 }}>
              <label>{app.t("Agent id", "Agent id")}</label>
              <input
                className="input mono"
                value={config.id}
                disabled={!isNew}
                placeholder="coding-agent"
                onChange={(event) => onPatch({ id: event.target.value })}
              />
            </div>
            <div className={`field${changed("name") ? " agent-change-highlight" : ""}`} style={{ flex: 1 }}>
              <label>{app.t("Name", "名称")}</label>
              <input
                className="input"
                value={config.name ?? ""}
                placeholder="Coding Assistant"
                onChange={(event) => onPatch({ name: event.target.value })}
              />
            </div>
          </div>
          <div className={changed("model") ? "agent-change-highlight" : undefined}>
            <AgentModelSelectionEditor
              model={config.model}
              readyModels={readyModels}
              allModels={allModels}
              runtimes={runtimes}
              onChange={(model) => onPatch({ model })}
              onManage={onManageModels}
            />
          </div>
          <AgentModelRuntimeControls
            config={config}
            acp={isAcpModelSelection(config.model)}
            onPatch={onPatch}
          />
          <TextField
            label={app.t("Description", "描述")}
            hint={app.t(
              "Shown to orchestrators for delegation. It is not part of the system prompt.",
              "供编排器委派时使用，不会进入系统提示词。",
            )}
            value={config.description ?? ""}
            onChange={(event) => onPatch({ description: event.target.value })}
          />
          <div className={changed("system") ? "agent-change-highlight" : undefined}>
            <TextAreaField
              label={app.t("System instructions", "系统指令")}
              hint={app.t(
                "The Agent's durable role and behavior. Resource-specific guidance belongs under Memory & resources.",
                "Agent 的长期职责与行为。资源专属说明应放在 Memory 与资源中。",
              )}
              mono
              rows={9}
              value={config.system ?? ""}
              onChange={(event) => onPatch({ system: event.target.value })}
            />
          </div>
          <div className="row" style={{ alignItems: "flex-start" }}>
            <TextField
              label={app.t("Max steps", "最大步数")}
              mono
              type="number"
              min={1}
              style={{ width: 150 }}
              value={config.max_steps}
              onChange={(event) => onPatch({ max_steps: Math.max(1, Number(event.target.value) || 1) })}
            />
            <div className="field" style={{ flex: 1 }}>
              <label>{app.t("Context window policy", "上下文窗口策略")}</label>
              <Segmented
                options={[
                  { value: "keep_all", label: app.t("Keep all", "全部保留") },
                  { value: "keep_last", label: app.t("Keep last N", "保留最近 N 条") },
                ]}
                value={config.context_policy.kind}
                onChange={(kind) => onPatch({
                  context_policy: (
                    kind === "keep_all"
                      ? { kind: "keep_all" }
                      : { kind: "keep_last", keep_last: 20 }
                  ) as ContextPolicy,
                })}
              />
            </div>
            {config.context_policy.kind === "keep_last" && (
              <TextField
                label={app.t("Messages kept", "保留消息数")}
                mono
                type="number"
                min={0}
                style={{ width: 150 }}
                value={config.context_policy.keep_last}
                onChange={(event) => onPatch({
                  context_policy: {
                    kind: "keep_last",
                    keep_last: Math.max(0, Number(event.target.value) || 0),
                  },
                })}
              />
            )}
          </div>
          {behavior("compact") && (
            <div className="field" style={{ marginTop: 14 }}>
              <label>{app.t("Context management", "上下文管理")}</label>
              {renderBehavior("compact")}
            </div>
          )}
        </Card>
      )}

      {section === "tools" && (
        <Card>
          <div className="field">
            <label>{app.t("Executable tools", "可执行工具")}</label>
            <span className="mut">{app.t(
              "Choose the tools this Agent may call. Tools supplied by connected MCP servers also appear here after discovery.",
              "选择此 Agent 可以调用的工具。已连接 MCP Server 提供的工具在发现后也会显示在这里。",
            )}</span>
            <CheckPicker
              options={[
                ...tools,
                ...selectedToolIds
                  .filter((id) => !tools.some((tool) => tool.id === id))
                  .map((id) => ({ id })),
              ]}
              selected={selectedToolIds}
              onChange={(value) => {
                if (!agentToolset) return;
                onPatch({
                  tools: withSelectedAgentTools(
                    config.tools,
                    value,
                    agentToolset.members,
                    agentToolset.default_config.permission_policy,
                  ),
                });
              }}
              empty={app.t("No tools advertised.", "没有可用工具。")}
            />
          </div>
          <div className="field">
            <label>{app.t("Permission preset", "权限预设")}</label>
            <span className="mut">{app.t(
              `Controlled modifications are projected by the server from its typed ${controlledMemberDescription} authority when you save.`,
              `保存时，服务端会依据 typed ${controlledMemberDescription} 权威投影受控修改策略。`,
            )}</span>
            <div>
              <Button
                variant="ghost"
                disabled={!controlledPresetAvailable}
                onClick={onApplyControlledModifications}
              >
                {app.t("Apply controlled modifications", "应用受控修改")}
              </Button>
            </div>
          </div>
          <div className="field">
            <label>{app.t("Tool behavior policies", "工具行为策略")}</label>
            <span className="mut">{app.t(
              "Use one canonical tool id to configure presentation and background eligibility. State-machine rules use the same ids and patterns under Advanced → Orchestration.",
              "使用同一规范工具 ID 配置呈现方式与后台执行资格；状态机规则在“高级 → 编排”中使用相同 ID 和模式。",
            )}</span>
            <ToolOverridesEditor
              tools={selectedToolIds}
              value={toolPolicyOverrides}
              onChange={(value) => onPatch({ tool_overrides: value })}
              backgroundTools={backgroundTools}
              backgroundUnavailable={acp}
              onBackgroundToolsChange={(next) => {
                const { background_task: _removed, ...otherPluginConfig } = config.plugin_config;
                onPatch(next.length > 0 ? {
                  plugins: Array.from(new Set([...config.plugins, "background_task"])),
                  plugin_config: {
                    ...config.plugin_config,
                    background_task: { ...backgroundConfig, tools: next },
                  },
                } : {
                  plugins: config.plugins.filter((plugin) => plugin !== "background_task"),
                  plugin_config: otherPluginConfig,
                });
              }}
            />
            {acp && (
              <div className="banner info" style={{ marginTop: 8 }}>
                <span>ⓘ</span>
                <span>{app.t(
                  "The selected ACP Harness cannot start new background tool executions. Existing selections remain visible so you can remove them before publishing.",
                  "所选 ACP Harness 不能启动新的后台工具执行；已有选择仍保持可见，以便在发布前移除。",
                )}</span>
              </div>
            )}
          </div>
          {behavior("web_search") && (
            <div className="field">
              <label>{app.t("Web capability", "网页能力")}</label>
              {renderBehavior("web_search")}
            </div>
          )}
          <AgentAdvancedToolsEditor config={config} onPatch={onPatch} onValidityChange={setToolsValid} />
        </Card>
      )}

      {section === "integrations" && (
        <AgentIntegrationsEditor
          section="bindings"
          config={config}
          credentials={credentials}
          toolsetCapabilities={toolsets}
          onChange={onPatch}
          onValidityChange={setIntegrationsValid}
        />
      )}

      {section === "knowledge" && (
        <>
          {resourcesError ? (
            <div className="banner warn">
              <span>⚠</span>
              <span>{resourcesError.message}</span>
              <Button variant="ghost" onClick={onRetryResources}>{app.t("Retry", "重试")}</Button>
            </div>
          ) : (
            <Card>
              <ResourcesTab
                inputs={resources}
                defaultMounts={resourceInputDefaults}
                onChange={onResourcesChange}
              />
            </Card>
          )}
          {behavior("memory") && (
            <div className="field">
              <label>{app.t("Memory behavior", "Memory 行为")}</label>
              {renderBehavior("memory")}
            </div>
          )}
        </>
      )}
    </div>
  );
}
