import type {
  AgentConfig,
  CredentialSource,
  InputBinding,
  PluginCap,
} from "../../lib/api/types";
import type { JsonSchema } from "../ui";
import { Button, Card, Pill } from "../ui";
import { useApp } from "../../lib/app-state";
import type { AdvancedSection } from "./agent-editor-navigation";
import AgentIntegrationsEditor from "./AgentIntegrationsEditor";
import AgentRawEditor from "./AgentRawEditor";
import BehaviorCard from "./BehaviorCard";
import ConfigDiff from "./ConfigDiff";
import PublicationSnapshotSummary from "./PublicationSnapshotSummary";

const PRODUCTIZED_PLUGINS = new Set(["compact", "memory", "web_search", "state_machine"]);

export default function AgentAdvanced({
  section,
  config,
  baseline,
  resources,
  plugins,
  credentials,
  resourceRevision,
  published,
  publishPending,
  changed,
  onSectionChange,
  onPatch,
  onRawChange,
  onValidityChange,
  onPublish,
}: {
  section: AdvancedSection;
  config: AgentConfig;
  baseline: Partial<AgentConfig>;
  resources: InputBinding[];
  plugins: PluginCap[];
  credentials: CredentialSource[];
  resourceRevision: number;
  published: boolean;
  publishPending: boolean;
  changed: (path: string) => boolean;
  onSectionChange: (section: AdvancedSection) => void;
  onPatch: (patch: Partial<AgentConfig>) => void;
  onRawChange: (config: AgentConfig) => void;
  onValidityChange: (valid: boolean) => void;
  onPublish: () => void;
}) {
  const app = useApp();
  const sections: Array<{ key: AdvancedSection; label: string; zh: string }> = [
    { key: "orchestration", label: "Orchestration", zh: "编排" },
    { key: "extensions", label: "Plugin configuration", zh: "Plugin 配置" },
    { key: "source", label: "Raw configuration", zh: "原始配置" },
    { key: "release", label: "Release & diff", zh: "发布与差异" },
  ];
  const renderBehavior = (plugin: PluginCap) => (
    <BehaviorCard
      key={plugin.id}
      id={plugin.id}
      schema={plugin.config_schema as JsonSchema | undefined}
      enabled={config.plugins.includes(plugin.id)}
      config={(config.plugin_config[plugin.id] as Record<string, unknown>) ?? {}}
      credentials={credentials}
      changed={changed(`plugin_config.${plugin.id}`)}
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
  const stateMachine = plugins.find((plugin) => plugin.id === "state_machine");
  const extensionPlugins = plugins.filter((plugin) => !PRODUCTIZED_PLUGINS.has(plugin.id));
  const { generation: _generation, ...reviewedConfig } = config;
  const { generation: _baselineGeneration, ...reviewedBaseline } = baseline;

  return (
    <div className="editor-section">
      <div className="subsection-tabs" role="tablist" aria-label={app.t("Advanced sections", "高级分区")}>
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

      {section === "orchestration" && (
        <>
          {stateMachine && renderBehavior(stateMachine)}
          <AgentIntegrationsEditor
            section="topology"
            config={config}
            credentials={credentials}
            onChange={onPatch}
            onValidityChange={onValidityChange}
          />
        </>
      )}

      {section === "extensions" && (
        <Card>
          <h2 className="section-title">{app.t("Runtime extensions", "运行时扩展")}</h2>
          <p className="hint">{app.t(
            "Low-frequency runtime mechanisms live here. Common capabilities remain in Build under the task they serve.",
            "低频运行机制集中在这里；常用能力仍按用户任务保留在构建页。",
          )}</p>
          {extensionPlugins.length > 0 ? (
            <div className="stack">{extensionPlugins.map(renderBehavior)}</div>
          ) : (
            <div className="banner info">
              <span>✓</span>
              <span>{app.t(
                "No additional runtime plugins are advertised by this host.",
                "当前 Host 没有提供其他运行时 Plugin。",
              )}</span>
            </div>
          )}
        </Card>
      )}

      {section === "source" && (
        <Card>
          <AgentRawEditor
            value={config}
            onChange={onRawChange}
            onValidityChange={onValidityChange}
          />
        </Card>
      )}

      {section === "release" && (
        <Card>
          <div className="row" style={{ justifyContent: "space-between", alignItems: "flex-start" }}>
            <div>
              <h2 className="section-title">{app.t("Draft versus published release", "草稿与已发布版本")}</h2>
              <p className="hint">{app.t(
                "The same reviewed diff is always shown again at the Publish checkpoint.",
                "点击发布时仍会再次展示同一份审阅差异。",
              )}</p>
            </div>
            <Pill tone={published ? "ok" : "neutral"}>
              {published ? app.t("published", "已发布") : app.t("draft only", "仅草稿")}
            </Pill>
          </div>
          <PublicationSnapshotSummary
            sourceRevision={config.generation}
            resourceRevision={resourceRevision}
            resources={resources}
          />
          <div style={{ marginTop: 16 }}>
            <strong>{app.t("Configuration changes", "配置差异")}</strong>
            <div style={{ marginTop: 10 }}>
              <ConfigDiff before={reviewedBaseline} after={reviewedConfig} />
            </div>
          </div>
          <div className="row" style={{ justifyContent: "flex-end", marginTop: 16 }}>
            <Button variant="primary" disabled={publishPending} onClick={onPublish}>
              {app.t("Review & publish", "审阅并发布")} ➤
            </Button>
          </div>
        </Card>
      )}
    </div>
  );
}
