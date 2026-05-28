import { type AgentSpec, type Capabilities } from "@/lib/config-api";
import { PluginConfigWorkspace } from "@/components/plugin-config-workspace";
import { pluginConfigDisplayName, pluginDisplayName } from "@/lib/plugin-config";
import { partitionActiveHookFilter } from "@/lib/agent-editor-helpers";

export function PluginsPanel({
  spec,
  capabilities,
  configurablePlugins,
  visiblePluginSchemas,
  activePluginConfig,
  setActivePluginConfig,
  togglePlugin,
  updateSection,
  updateField,
}: {
  spec: AgentSpec;
  capabilities: Capabilities | null;
  configurablePlugins: NonNullable<Capabilities["plugins"]>;
  visiblePluginSchemas: Parameters<typeof PluginConfigWorkspace>[0]["entries"];
  activePluginConfig: string | null;
  setActivePluginConfig: (next: string | null) => void;
  togglePlugin: (pluginId: string) => void;
  updateSection: (key: string, value: unknown) => void;
  updateField: <K extends keyof AgentSpec>(key: K, value: AgentSpec[K]) => void;
}) {
  if (!capabilities || capabilities.plugins.length === 0) {
    return (
      <div className="rounded-sm border border-dashed border-line bg-surface p-6 text-sm text-fg-soft">
        No plugins are currently registered.
      </div>
    );
  }
  return (
    <section className="rounded-sm border border-line bg-surface p-5 shadow-sm">
      <h3 className="text-lg font-semibold text-fg-strong">Plugins</h3>
      <p className="mt-2 text-sm text-fg-soft">
        Enable agent plugins here. Plugins with agent-level settings expose their configuration
        forms below.
      </p>
      <div className="mt-4 grid gap-3 md:grid-cols-2 xl:grid-cols-3">
        {capabilities.plugins.map((plugin) => (
          <label
            key={plugin.id}
            className="rounded-sm border border-line bg-soft px-4 py-3 text-sm text-fg"
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
                        .map((schema) => pluginConfigDisplayName(plugin.id, schema))
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
        <div className="mt-4 rounded-sm border border-dashed border-line px-4 py-3 text-sm text-fg-soft">
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

      <ActiveHookFilterSection
        pluginIds={spec.plugin_ids ?? []}
        value={spec.active_hook_filter ?? []}
        onChange={(next) =>
          // Empty filter == "all hooks run" on the wire (Rust marks the
          // field `skip_serializing_if = "is_empty"`), so writing back
          // `undefined` keeps the spec field absent rather than promoting
          // a default into an explicit `[]` override on customized PATCH.
          updateField("active_hook_filter", next.length === 0 ? undefined : next)
        }
      />
    </section>
  );
}

function ActiveHookFilterSection({
  pluginIds,
  value,
  onChange,
}: {
  pluginIds: string[];
  value: string[];
  onChange: (next: string[]) => void;
}) {
  const filtering = value.length > 0;
  const valueSet = new Set(value);
  const { stale } = partitionActiveHookFilter(value, pluginIds);

  function setMode(next: "all" | "filter") {
    if (next === "all") {
      onChange([]);
    } else if (value.length === 0) {
      onChange([...pluginIds]);
    }
  }

  function toggle(pluginId: string, checked: boolean) {
    if (!checked && value.length === 1 && value[0] === pluginId) {
      return;
    }
    const next = checked
      ? Array.from(new Set([...value, pluginId]))
      : value.filter((id) => id !== pluginId);
    onChange(next);
  }

  const isLastSelection = value.length === 1;

  function clearStaleEntries() {
    if (stale.length === 0) return;
    const staleSet = new Set(stale);
    onChange(value.filter((id) => !staleSet.has(id)));
  }

  return (
    <div className="mt-6 border-t border-line pt-5">
      <div className="flex flex-wrap items-start justify-between gap-3">
        <div>
          <h4 className="text-base font-semibold text-fg-strong">Active hook filter</h4>
          <p className="mt-2 max-w-xl text-sm text-fg-soft">
            Restricts runtime hooks to a chosen subset of loaded plugins. Default is "all" — every
            loaded plugin's hooks run. Switch to "Filter" to allow only the plugins you select below
            to execute hooks. Plugins not in the filter remain loaded but their hooks are skipped.
          </p>
        </div>
        <fieldset aria-label="Active hook filter mode" className="flex shrink-0 gap-2">
          <label
            className={[
              "min-w-[8rem] cursor-pointer rounded-sm border px-3 py-2 text-sm transition",
              !filtering
                ? "border-accent bg-accent text-accent-text shadow-sm"
                : "border-line bg-surface text-fg hover:border-line-strong hover:bg-soft",
            ].join(" ")}
          >
            <input
              type="radio"
              className="sr-only"
              checked={!filtering}
              onChange={() => setMode("all")}
            />
            <div className="font-semibold">All plugins</div>
            <div
              className={[
                "mt-0.5 text-xs leading-5",
                !filtering ? "text-fg-faint" : "text-fg-soft",
              ].join(" ")}
            >
              Default — no filtering.
            </div>
          </label>
          <label
            className={[
              "min-w-[8rem] cursor-pointer rounded-sm border px-3 py-2 text-sm transition",
              filtering
                ? "border-accent bg-accent text-accent-text shadow-sm"
                : "border-line bg-surface text-fg hover:border-line-strong hover:bg-soft",
            ].join(" ")}
          >
            <input
              type="radio"
              className="sr-only"
              checked={filtering}
              onChange={() => setMode("filter")}
            />
            <div className="font-semibold">Filter</div>
            <div
              className={[
                "mt-0.5 text-xs leading-5",
                filtering ? "text-fg-faint" : "text-fg-soft",
              ].join(" ")}
            >
              Only listed plugins' hooks run.
            </div>
          </label>
        </fieldset>
      </div>

      {filtering ? (
        pluginIds.length === 0 ? (
          <div className="mt-4 rounded-sm border border-dashed border-line px-4 py-3 text-sm text-fg-soft">
            No plugins are enabled. Enable plugins above before filtering hooks.
          </div>
        ) : (
          <>
            <div className="mt-4 grid gap-2 md:grid-cols-2 xl:grid-cols-3">
              {pluginIds.map((pluginId) => {
                const checked = valueSet.has(pluginId);
                const isLastChecked = checked && isLastSelection;
                return (
                  <label
                    key={pluginId}
                    className="flex items-center gap-2 rounded-sm border border-line bg-soft px-3 py-2 text-sm text-fg"
                    title={
                      isLastChecked
                        ? "Filter mode requires at least one plugin — switch to All plugins to disable filtering."
                        : undefined
                    }
                  >
                    <input
                      type="checkbox"
                      checked={checked}
                      disabled={isLastChecked}
                      onChange={(event) => toggle(pluginId, event.target.checked)}
                    />
                    <span className="font-mono text-xs text-fg-strong">{pluginId}</span>
                  </label>
                );
              })}
            </div>
            {isLastSelection ? (
              <p
                className="mt-2 text-xs text-fg-soft"
                data-testid="active-hook-filter-last-entry-hint"
              >
                Filter mode requires at least one plugin. Switch to <em>All plugins</em> above to
                disable filtering entirely.
              </p>
            ) : null}
          </>
        )
      ) : null}

      {stale.length > 0 ? (
        <div
          className="mt-4 rounded-sm border border-tone-warn/40 bg-tone-warn/10 p-3 text-sm text-fg"
          data-testid="active-hook-filter-stale-warning"
        >
          <div className="flex items-start justify-between gap-3">
            <div>
              <div className="font-semibold text-tone-warn">
                Stale filter entries ({stale.length})
              </div>
              <p className="mt-1 text-xs text-fg-soft">
                The following ids are still in <span className="font-mono">active_hook_filter</span>{" "}
                but no longer match any enabled plugin, so they gate nothing at runtime. They will
                be saved as-is unless cleared.
              </p>
              <div
                className="mt-2 flex flex-wrap gap-1"
                data-testid="active-hook-filter-stale-chips"
              >
                {stale.map((id) => (
                  <span
                    key={id}
                    className="rounded-full bg-surface px-2 py-0.5 font-mono text-xs text-fg-strong"
                  >
                    {id}
                  </span>
                ))}
              </div>
            </div>
            <button
              type="button"
              onClick={clearStaleEntries}
              className="shrink-0 rounded-sm border border-line-strong bg-surface px-3 py-1 text-xs font-medium text-fg-soft transition hover:bg-soft"
            >
              Clear stale entries
            </button>
          </div>
        </div>
      ) : null}
    </div>
  );
}
