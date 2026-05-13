import { type AgentSpec, type Capabilities } from "@/lib/config-api";
import { PluginConfigWorkspace } from "@/components/plugin-config-workspace";
import { pluginDisplayName } from "@/lib/plugin-config";

export function PluginsPanel({
  spec,
  capabilities,
  configurablePlugins,
  visiblePluginSchemas,
  activePluginConfig,
  setActivePluginConfig,
  togglePlugin,
  updateSection,
}: {
  spec: AgentSpec;
  capabilities: Capabilities | null;
  configurablePlugins: NonNullable<Capabilities["plugins"]>;
  visiblePluginSchemas: Parameters<typeof PluginConfigWorkspace>[0]["entries"];
  activePluginConfig: string | null;
  setActivePluginConfig: (next: string | null) => void;
  togglePlugin: (pluginId: string) => void;
  updateSection: (key: string, value: unknown) => void;
}) {
  if (!capabilities || capabilities.plugins.length === 0) {
    return (
      <div className="rounded-md border border-dashed border-line bg-surface p-6 text-sm text-fg-soft">
        No plugins are currently registered.
      </div>
    );
  }
  return (
    <section className="rounded-md border border-line bg-surface p-5 shadow-sm">
      <h3 className="text-lg font-semibold text-fg-strong">Plugins</h3>
      <p className="mt-2 text-sm text-fg-soft">
        Enable agent plugins here. Plugins with agent-level settings expose their configuration
        forms below.
      </p>
      <div className="mt-4 grid gap-3 md:grid-cols-2 xl:grid-cols-3">
        {capabilities.plugins.map((plugin) => (
          <label
            key={plugin.id}
            className="rounded-xl border border-line bg-soft px-4 py-3 text-sm text-fg"
          >
            <div className="flex items-start gap-3">
              <input
                type="checkbox"
                checked={(spec.plugin_ids ?? []).includes(plugin.id)}
                onChange={() => togglePlugin(plugin.id)}
              />
              <div>
                <div className="flex flex-wrap items-center gap-2">
                  <div className="font-mono text-fg-strong">{pluginDisplayName(plugin.id)}</div>
                  <span className="rounded-full bg-muted px-2 py-0.5 text-xs font-medium text-fg-soft">
                    {plugin.id}
                  </span>
                  {plugin.config_schemas.length > 0 ? (
                    <span className="rounded-full bg-tone-success/15 px-2 py-0.5 text-xs font-medium text-tone-success">
                      Configurable
                    </span>
                  ) : null}
                </div>
                <div className="mt-1 text-fg-soft">
                  {plugin.config_schemas.length === 0
                    ? "No agent-level config sections"
                    : `Config sections: ${plugin.config_schemas
                        .map((schema) => schema.key)
                        .join(", ")}`}
                </div>
              </div>
            </div>
          </label>
        ))}
      </div>

      <div className="mt-6 border-t border-line pt-5">
        <h4 className="text-base font-semibold text-fg-strong">Plugin Configuration</h4>
        <p className="mt-2 text-sm text-fg-soft">
          Existing saved sections stay visible even if a plugin is currently disabled, so you can
          inspect and edit them before re-enabling the plugin.
        </p>
      </div>

      {configurablePlugins.length === 0 ? (
        <div className="mt-4 rounded-md border border-dashed border-line px-4 py-3 text-sm text-fg-soft">
          No registered plugins expose agent-level configuration.
        </div>
      ) : (
        <PluginConfigWorkspace
          entries={visiblePluginSchemas}
          sections={spec.sections ?? {}}
          activeEntryKey={activePluginConfig}
          onSelectEntry={setActivePluginConfig}
          onUpdateSection={updateSection}
        />
      )}
    </section>
  );
}
