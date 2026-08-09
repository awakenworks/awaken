import type {
  AgentConfig,
  ContextPolicy,
  CredentialSource,
  InputBinding,
  PermissionConfig,
  PluginCap,
  RuntimeCap,
} from "../../lib/api/types";
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
import AgentModelSelectionEditor from "./AgentModelSelectionEditor";
import BehaviorCard from "./BehaviorCard";
import PermissionEditor from "./PermissionEditor";
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
  plugins,
  policies,
  credentials,
  resources,
  resourcesError,
  changed,
  onSectionChange,
  onPatch,
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
  plugins: PluginCap[];
  policies: Array<{ id: string }>;
  credentials: CredentialSource[];
  resources: InputBinding[];
  resourcesError?: Error;
  changed: (path: string) => boolean;
  onSectionChange: (section: BuilderSection) => void;
  onPatch: (patch: Partial<AgentConfig>) => void;
  onManageModels: () => void;
  onResourcesChange: (inputs: InputBinding[]) => void;
  onRetryResources: () => void;
  onValidityChange: (valid: boolean) => void;
}) {
  const app = useApp();
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
              "Choose host tools. Runtime-discovered MCP tools are presented by canonical id below.",
              "选择 Host 工具。运行时发现的 MCP 工具可在下方按规范 id 配置。",
            )}</span>
            <CheckPicker
              options={[
                ...tools,
                ...config.tools
                  .filter((id) => !tools.some((tool) => tool.id === id))
                  .map((id) => ({ id })),
              ]}
              selected={config.tools}
              onChange={(value) => onPatch({ tools: value })}
              empty={app.t("No tools advertised.", "没有可用工具。")}
            />
          </div>
          <div className="field">
            <label>{app.t("Tool presentation", "工具呈现")}</label>
            <ToolOverridesEditor
              tools={config.tools}
              value={config.tool_overrides ?? []}
              onChange={(value) => onPatch({ tool_overrides: value })}
            />
          </div>
          {behavior("web_search") && (
            <div className="field">
              <label>{app.t("Web capability", "网页能力")}</label>
              {renderBehavior("web_search")}
            </div>
          )}
          {policies.some((policy) => policy.id === "permission") && (
            <div className="field">
              <label>{app.t("Permissions", "权限")}</label>
              <span className="mut">{app.t(
                "Every tool call is evaluated against the default decision and ordered rules.",
                "每次工具调用都会按默认裁决和有序规则执行权限判断。",
              )}</span>
              <PermissionEditor
                value={(config.plugin_config.permission as PermissionConfig) ?? {}}
                onChange={(value) => onPatch({
                  plugin_config: { ...config.plugin_config, permission: value },
                })}
              />
            </div>
          )}
        </Card>
      )}

      {section === "integrations" && (
        <AgentIntegrationsEditor
          section="bindings"
          config={config}
          credentials={credentials}
          onChange={onPatch}
          onValidityChange={onValidityChange}
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
              <ResourcesTab inputs={resources} onChange={onResourcesChange} />
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
