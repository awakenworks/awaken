import { type AgentSpec, type Capabilities } from "@/lib/config-api";
import { AGENT_EDITOR_TABS, type AgentEditorTabId } from "@/lib/editor-tabs";
import { type reasoningEffortMode } from "@/lib/reasoning-effort";
import { BasicsPanel } from "./panels/basics-panel";
import { ToolsPanel } from "./panels/tools-panel";
import { PluginsPanel } from "./panels/plugins-panel";
import { DelegatesPanel } from "./panels/delegates-panel";
import { AdvancedPanel } from "./panels/advanced-panel";
import { HistoryPanel } from "./panels/history-panel";
import { type AgentSaveMode } from "./spec-helpers";

type VisiblePluginSchemas = Parameters<typeof PluginsPanel>[0]["visiblePluginSchemas"];

export function AgentEditorPanels({
  spec,
  capabilities,
  isNew,
  activeTab,
  updateField,
  reasoningMode,
  errors,
  canResetFields,
  overriddenFields,
  onResetField,
  configurablePlugins,
  visiblePluginSchemas,
  activePluginConfig,
  setActivePluginConfig,
  togglePlugin,
  updateSection,
  toggleDelegate,
  historyRefreshKey,
  saveMode,
  savePayload,
  onSpecRestored,
}: {
  spec: AgentSpec;
  capabilities: Capabilities | null;
  isNew: boolean;
  activeTab: AgentEditorTabId;
  updateField: <K extends keyof AgentSpec>(key: K, value: AgentSpec[K]) => void;
  reasoningMode: ReturnType<typeof reasoningEffortMode>;
  errors: Partial<Record<"id" | "model_id", string>>;
  canResetFields: boolean;
  overriddenFields: Set<string>;
  onResetField: (field: string) => void;
  configurablePlugins: NonNullable<Capabilities["plugins"]>;
  visiblePluginSchemas: VisiblePluginSchemas;
  activePluginConfig: string | null;
  setActivePluginConfig: (next: string | null) => void;
  togglePlugin: (pluginId: string) => void;
  updateSection: (key: string, value: unknown) => void;
  toggleDelegate: (delegateId: string, checked: boolean) => void;
  historyRefreshKey: number;
  saveMode: AgentSaveMode;
  savePayload: AgentSpec | Record<string, unknown>;
  onSpecRestored: (updated: AgentSpec) => void | Promise<void>;
}) {
  return (
    <div className="space-y-6">
      {AGENT_EDITOR_TABS.map((tab) => (
        <div
          key={tab.id}
          role="tabpanel"
          id={`panel-${tab.id}`}
          aria-labelledby={`tab-${tab.id}`}
          tabIndex={0}
          hidden={activeTab !== tab.id}
        >
          {tab.id === "basics" && (
            <BasicsPanel
              spec={spec}
              capabilities={capabilities}
              isNew={isNew}
              updateField={updateField}
              reasoningMode={reasoningMode}
              errors={errors}
              canResetFields={canResetFields}
              overriddenFields={overriddenFields}
              onResetField={onResetField}
            />
          )}
          {tab.id === "tools" && (
            <ToolsPanel spec={spec} capabilities={capabilities} updateField={updateField} />
          )}
          {tab.id === "plugins" && (
            <PluginsPanel
              spec={spec}
              capabilities={capabilities}
              configurablePlugins={configurablePlugins}
              visiblePluginSchemas={visiblePluginSchemas}
              activePluginConfig={activePluginConfig}
              setActivePluginConfig={setActivePluginConfig}
              togglePlugin={togglePlugin}
              updateSection={updateSection}
            />
          )}
          {tab.id === "delegates" && (
            <DelegatesPanel
              spec={spec}
              capabilities={capabilities}
              toggleDelegate={toggleDelegate}
            />
          )}
          {tab.id === "advanced" && (
            <AdvancedPanel saveMode={saveMode} savePayload={savePayload} />
          )}
          {tab.id === "history" && (
            <HistoryPanel
              spec={spec}
              isNew={isNew}
              refreshKey={historyRefreshKey}
              onSpecRestored={onSpecRestored}
            />
          )}
        </div>
      ))}
    </div>
  );
}
