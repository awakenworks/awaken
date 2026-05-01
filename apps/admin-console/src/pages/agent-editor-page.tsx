import { useEffect, useMemo, useState } from "react";
import {
  Link,
  useNavigate,
  useParams,
  useSearchParams,
} from "react-router";
import { type AgentSpec, type Capabilities, configApi } from "@/lib/config-api";
import { Field } from "@/components/form-components";
import { AgentPreviewPanel } from "@/components/agent-preview-panel";
import { PluginConfigWorkspace } from "@/components/plugin-config-workspace";
import { useToast } from "@/components/toast-provider";
import { useUnsavedChangesGuard } from "@/components/unsaved-changes-guard";
import {
  AGENT_EDITOR_TABS,
  type AgentEditorTabId,
  readTabFromSearch,
  writeTabToSearch,
} from "@/lib/editor-tabs";
import { isToolAllowed, nextAllowedTools } from "@/lib/agent-tool-selection";
import { pluginConfigEntryKey, pluginDisplayName } from "@/lib/plugin-config";
import { adminRoutes } from "@/lib/routes";

const EMPTY_AGENT: AgentSpec = {
  id: "",
  model_id: "",
  system_prompt: "",
  max_rounds: 16,
  max_continuation_retries: 2,
  plugin_ids: [],
  sections: {},
  delegates: [],
};

export function AgentEditorPage() {
  const navigate = useNavigate();
  const { id } = useParams();
  const isNew = id === "new";

  const [searchParams, setSearchParams] = useSearchParams();
  const activeTab = readTabFromSearch(searchParams);
  const setActiveTab = (next: AgentEditorTabId) => {
    setSearchParams(writeTabToSearch(searchParams, next), { replace: true });
  };

  const [spec, setSpec] = useState<AgentSpec>({ ...EMPTY_AGENT });
  const [savedSpec, setSavedSpec] = useState<AgentSpec | null>(null);
  const [capabilities, setCapabilities] = useState<Capabilities | null>(null);
  const [loading, setLoading] = useState(!isNew);
  const [saving, setSaving] = useState(false);
  const [activePluginConfig, setActivePluginConfig] = useState<string | null>(null);
  const toast = useToast();

  const isDirty = useMemo(() => {
    if (saving) return false;
    if (isNew) {
      return (
        spec.id.trim().length > 0 ||
        spec.system_prompt.length > 0 ||
        spec.model_id.length > 0 ||
        (spec.plugin_ids?.length ?? 0) > 0
      );
    }
    if (!savedSpec) return false;
    return JSON.stringify(spec) !== JSON.stringify(savedSpec);
  }, [spec, savedSpec, isNew, saving]);

  useUnsavedChangesGuard({ enabled: isDirty });

  useEffect(() => {
    let cancelled = false;

    async function loadCapabilities() {
      try {
        const nextCapabilities = await configApi.capabilities();
        if (!cancelled) {
          setCapabilities(nextCapabilities);
        }
      } catch (loadError) {
        if (!cancelled) {
          toast.error(
            loadError instanceof Error ? loadError.message : String(loadError),
          );
        }
      }
    }

    async function loadAgent() {
      if (isNew || !id) {
        setLoading(false);
        return;
      }

      setLoading(true);
      try {
        const nextSpec = await configApi.get<AgentSpec>("agents", id);
        if (!cancelled) {
          const hydrated = {
            sections: {},
            plugin_ids: [],
            delegates: [],
            ...nextSpec,
          };
          setSpec(hydrated);
          setSavedSpec(hydrated);
        }
      } catch (loadError) {
        if (!cancelled) {
          toast.error(
            loadError instanceof Error ? loadError.message : String(loadError),
          );
        }
      } finally {
        if (!cancelled) {
          setLoading(false);
        }
      }
    }

    void Promise.all([loadCapabilities(), loadAgent()]);

    return () => {
      cancelled = true;
    };
  }, [id, isNew]);

  async function handleSave() {
    setSaving(true);
    try {
      const payload = {
        ...spec,
        plugin_ids: [...(spec.plugin_ids ?? [])],
        delegates: [...(spec.delegates ?? [])],
      };

      if (isNew) {
        const created = await configApi.create<typeof payload, AgentSpec>(
          "agents",
          payload,
        );
        setSavedSpec(created);
        toast.success(`Agent "${created.id}" created`);
        navigate(adminRoutes.agent(created.id), { replace: true });
      } else {
        const updated = await configApi.update<typeof payload, AgentSpec>(
          "agents",
          spec.id,
          payload,
        );
        setSpec(updated);
        setSavedSpec(updated);
        toast.success(`Agent "${updated.id}" saved`);
      }
    } catch (saveError) {
      toast.error(
        saveError instanceof Error ? saveError.message : String(saveError),
      );
    } finally {
      setSaving(false);
    }
  }

  function updateField<K extends keyof AgentSpec>(key: K, value: AgentSpec[K]) {
    setSpec((current) => ({ ...current, [key]: value }));
  }

  function togglePlugin(pluginId: string) {
    setSpec((current) => {
      const pluginIds = current.plugin_ids ?? [];
      const next = pluginIds.includes(pluginId)
        ? pluginIds.filter((idValue) => idValue !== pluginId)
        : [...pluginIds, pluginId];

      return {
        ...current,
        plugin_ids: next,
      };
    });
  }

  function updateSection(key: string, value: unknown) {
    setSpec((current) => {
      const sections = { ...(current.sections ?? {}) };
      const isEmptyObject =
        value &&
        typeof value === "object" &&
        !Array.isArray(value) &&
        Object.keys(value as Record<string, unknown>).length === 0;

      if (value === undefined || isEmptyObject) {
        delete sections[key];
      } else {
        sections[key] = value;
      }

      return {
        ...current,
        sections,
      };
    });
  }

  function toggleDelegate(delegateId: string, checked: boolean) {
    setSpec((current) => {
      const delegates = current.delegates ?? [];
      return {
        ...current,
        delegates: checked
          ? [...delegates, delegateId]
          : delegates.filter((value) => value !== delegateId),
      };
    });
  }

  function toggleAllowedTool(toolId: string, checked: boolean) {
    const allToolIds = (capabilities?.tools ?? []).map((tool) => tool.id);
    updateField(
      "allowed_tools",
      nextAllowedTools(spec.allowed_tools, allToolIds, toolId, checked),
    );
  }

  const configurablePlugins = (capabilities?.plugins ?? []).filter(
    (plugin) => plugin.config_schemas.length > 0,
  );
  const visiblePluginSchemas = configurablePlugins
    .flatMap((plugin) => {
      const selected = (spec.plugin_ids ?? []).includes(plugin.id);
      const hasStoredConfig = plugin.config_schemas.some(
        (schema) => spec.sections?.[schema.key] !== undefined,
      );

      return plugin.config_schemas.map((schema) => ({
        plugin,
        schema,
        selected,
        hasStoredConfig,
      }));
    })
    .sort((left, right) => {
      const leftRank = Number(left.selected) * 2 + Number(left.hasStoredConfig);
      const rightRank = Number(right.selected) * 2 + Number(right.hasStoredConfig);
      if (leftRank !== rightRank) {
        return rightRank - leftRank;
      }
      return left.plugin.id.localeCompare(right.plugin.id);
    });

  useEffect(() => {
    if (visiblePluginSchemas.length === 0) {
      setActivePluginConfig(null);
      return;
    }

    const activeEntryExists =
      activePluginConfig &&
      visiblePluginSchemas.some(
        (entry) =>
          pluginConfigEntryKey(entry.plugin.id, entry.schema.key) === activePluginConfig,
      );

    if (activeEntryExists) {
      return;
    }

    const preferredEntry = visiblePluginSchemas.find(
      (entry) => entry.selected || entry.hasStoredConfig,
    );
    if (preferredEntry) {
      setActivePluginConfig(
        pluginConfigEntryKey(preferredEntry.plugin.id, preferredEntry.schema.key),
      );
    } else {
      setActivePluginConfig(null);
    }
  }, [activePluginConfig, visiblePluginSchemas]);

  if (loading) {
    return (
      <div className="mx-auto max-w-6xl p-6 md:p-8">
        <div className="rounded-2xl border border-slate-200 bg-white p-6 text-sm text-slate-500 shadow-sm">
          Loading agent...
        </div>
      </div>
    );
  }

  return (
    <div className="mx-auto max-w-[96rem]">
      <StickyEditorHeader
        isNew={isNew}
        agentId={spec.id}
        isDirty={isDirty}
        saving={saving}
        onSave={() => void handleSave()}
        activeTab={activeTab}
        onTabChange={setActiveTab}
      />

      <div className="grid gap-6 px-6 py-6 md:px-8 xl:grid-cols-[minmax(0,1fr),24rem]">
        <div className="space-y-6">
          {activeTab === "basics" && (
            <BasicsPanel
              spec={spec}
              capabilities={capabilities}
              isNew={isNew}
              updateField={updateField}
            />
          )}

          {activeTab === "tools" && (
            <ToolsPanel
              spec={spec}
              capabilities={capabilities}
              toggleAllowedTool={toggleAllowedTool}
            />
          )}

          {activeTab === "plugins" && (
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

          {activeTab === "delegates" && (
            <DelegatesPanel
              spec={spec}
              capabilities={capabilities}
              toggleDelegate={toggleDelegate}
            />
          )}

          {activeTab === "advanced" && <AdvancedPanel spec={spec} />}
        </div>

        <AgentPreviewPanel draft={spec} />
      </div>
    </div>
  );
}

function StickyEditorHeader({
  isNew,
  agentId,
  isDirty,
  saving,
  onSave,
  activeTab,
  onTabChange,
}: {
  isNew: boolean;
  agentId: string;
  isDirty: boolean;
  saving: boolean;
  onSave: () => void;
  activeTab: AgentEditorTabId;
  onTabChange: (next: AgentEditorTabId) => void;
}) {
  return (
    <div className="sticky top-0 z-30 border-b border-slate-200 bg-white/95 px-6 pt-6 backdrop-blur md:px-8">
      <div className="flex flex-wrap items-center justify-between gap-4">
        <div className="min-w-0">
          <Link
            to={adminRoutes.agents}
            className="text-sm font-medium text-slate-500 transition hover:text-slate-700"
          >
            Back to agents
          </Link>
          <h2 className="mt-2 flex flex-wrap items-center gap-3 text-3xl font-semibold text-slate-950">
            <span>{isNew ? "New Agent" : `Edit ${agentId}`}</span>
            {isDirty ? (
              <span className="rounded-full bg-amber-100 px-2 py-0.5 text-xs font-medium uppercase tracking-wide text-amber-800">
                Unsaved changes
              </span>
            ) : !isNew ? (
              <span className="rounded-full bg-emerald-100 px-2 py-0.5 text-xs font-medium uppercase tracking-wide text-emerald-800">
                Up to date
              </span>
            ) : null}
          </h2>
        </div>
        <button
          type="button"
          onClick={onSave}
          disabled={saving || (!isDirty && !isNew)}
          className="rounded-xl bg-slate-950 px-4 py-2 text-sm font-medium text-white transition hover:bg-slate-800 disabled:cursor-not-allowed disabled:opacity-60"
        >
          {saving ? "Saving..." : "Save"}
        </button>
      </div>

      <nav
        aria-label="Editor sections"
        className="mt-4 flex gap-1 overflow-x-auto"
      >
        {AGENT_EDITOR_TABS.map((tab) => {
          const active = tab.id === activeTab;
          return (
            <button
              key={tab.id}
              type="button"
              onClick={() => onTabChange(tab.id)}
              aria-current={active ? "page" : undefined}
              className={[
                "shrink-0 rounded-t-lg border-b-2 px-4 py-3 text-sm font-medium transition",
                active
                  ? "border-slate-900 text-slate-950"
                  : "border-transparent text-slate-500 hover:text-slate-700",
              ].join(" ")}
            >
              {tab.label}
            </button>
          );
        })}
      </nav>
    </div>
  );
}

function BasicsPanel({
  spec,
  capabilities,
  isNew,
  updateField,
}: {
  spec: AgentSpec;
  capabilities: Capabilities | null;
  isNew: boolean;
  updateField: <K extends keyof AgentSpec>(key: K, value: AgentSpec[K]) => void;
}) {
  return (
    <section className="rounded-2xl border border-slate-200 bg-white p-5 shadow-sm">
      <h3 className="text-lg font-semibold text-slate-950">Basics</h3>
      <div className="mt-4 grid gap-4 md:grid-cols-2">
        <Field label="Agent ID">
          <input
            type="text"
            value={spec.id}
            disabled={!isNew}
            onChange={(event) => updateField("id", event.target.value)}
            className="w-full rounded-xl border border-slate-300 px-3 py-2 text-sm text-slate-900 outline-none transition focus:border-slate-500 disabled:bg-slate-100 disabled:text-slate-500"
          />
        </Field>
        <Field label="Model">
          <select
            value={String(spec.model_id ?? "")}
            onChange={(event) => updateField("model_id", event.target.value)}
            className="w-full rounded-xl border border-slate-300 bg-white px-3 py-2 text-sm text-slate-900 outline-none transition focus:border-slate-500"
          >
            <option value="">Select a model</option>
            {(capabilities?.models ?? []).map((model) => (
              <option key={model.id} value={model.id}>
                {model.id} ({model.upstream_model})
              </option>
            ))}
          </select>
        </Field>
        <Field label="Max rounds">
          <input
            type="number"
            min={1}
            value={Number(spec.max_rounds ?? 16)}
            onChange={(event) =>
              updateField("max_rounds", Number(event.target.value) || 16)
            }
            className="w-full rounded-xl border border-slate-300 px-3 py-2 text-sm text-slate-900 outline-none transition focus:border-slate-500"
          />
        </Field>
        <Field label="Max continuation retries">
          <input
            type="number"
            min={0}
            value={Number(spec.max_continuation_retries ?? 2)}
            onChange={(event) =>
              updateField(
                "max_continuation_retries",
                Number(event.target.value) || 0,
              )
            }
            className="w-full rounded-xl border border-slate-300 px-3 py-2 text-sm text-slate-900 outline-none transition focus:border-slate-500"
          />
        </Field>
      </div>

      <div className="mt-4">
        <Field label="System prompt">
          <textarea
            value={String(spec.system_prompt ?? "")}
            onChange={(event) => updateField("system_prompt", event.target.value)}
            rows={8}
            className="w-full rounded-xl border border-slate-300 px-3 py-2 font-mono text-sm text-slate-900 outline-none transition focus:border-slate-500"
          />
        </Field>
        <p className="mt-1 text-xs text-slate-500">
          {String(spec.system_prompt ?? "").length} characters
        </p>
      </div>
    </section>
  );
}

function ToolsPanel({
  spec,
  capabilities,
  toggleAllowedTool,
}: {
  spec: AgentSpec;
  capabilities: Capabilities | null;
  toggleAllowedTool: (toolId: string, checked: boolean) => void;
}) {
  if (!capabilities || capabilities.tools.length === 0) {
    return (
      <div className="rounded-2xl border border-dashed border-slate-200 bg-white p-6 text-sm text-slate-500">
        No tools are currently published. Once plugins or MCP servers register
        tools, they will appear here.
      </div>
    );
  }
  return (
    <section className="rounded-2xl border border-slate-200 bg-white p-5 shadow-sm">
      <h3 className="text-lg font-semibold text-slate-950">Allowed Tools</h3>
      <p className="mt-2 text-sm text-slate-500">
        Leaving every tool selected keeps the default runtime behavior.
      </p>
      <div className="mt-4 grid gap-3 md:grid-cols-2 xl:grid-cols-3">
        {capabilities.tools.map((tool) => {
          const checked = isToolAllowed(spec.allowed_tools, tool.id);
          return (
            <label
              key={tool.id}
              className="rounded-xl border border-slate-200 bg-slate-50 px-4 py-3 text-sm text-slate-700"
            >
              <div className="flex items-start gap-3">
                <input
                  type="checkbox"
                  checked={checked}
                  onChange={(event) =>
                    toggleAllowedTool(tool.id, event.target.checked)
                  }
                />
                <div>
                  <div className="font-mono text-slate-900">{tool.id}</div>
                  <div className="mt-1 text-slate-500">
                    {tool.description || tool.name}
                  </div>
                </div>
              </div>
            </label>
          );
        })}
      </div>
    </section>
  );
}

function PluginsPanel({
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
      <div className="rounded-2xl border border-dashed border-slate-200 bg-white p-6 text-sm text-slate-500">
        No plugins are currently registered.
      </div>
    );
  }
  return (
    <section className="rounded-2xl border border-slate-200 bg-white p-5 shadow-sm">
      <h3 className="text-lg font-semibold text-slate-950">Plugins</h3>
      <p className="mt-2 text-sm text-slate-500">
        Enable agent plugins here. Plugins with agent-level settings expose
        their configuration forms below.
      </p>
      <div className="mt-4 grid gap-3 md:grid-cols-2 xl:grid-cols-3">
        {capabilities.plugins.map((plugin) => (
          <label
            key={plugin.id}
            className="rounded-xl border border-slate-200 bg-slate-50 px-4 py-3 text-sm text-slate-700"
          >
            <div className="flex items-start gap-3">
              <input
                type="checkbox"
                checked={(spec.plugin_ids ?? []).includes(plugin.id)}
                onChange={() => togglePlugin(plugin.id)}
              />
              <div>
                <div className="flex flex-wrap items-center gap-2">
                  <div className="font-mono text-slate-900">
                    {pluginDisplayName(plugin.id)}
                  </div>
                  <span className="rounded-full bg-slate-200 px-2 py-0.5 text-xs font-medium text-slate-600">
                    {plugin.id}
                  </span>
                  {plugin.config_schemas.length > 0 ? (
                    <span className="rounded-full bg-emerald-100 px-2 py-0.5 text-xs font-medium text-emerald-700">
                      Configurable
                    </span>
                  ) : null}
                </div>
                <div className="mt-1 text-slate-500">
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

      <div className="mt-6 border-t border-slate-200 pt-5">
        <h4 className="text-base font-semibold text-slate-900">
          Plugin Configuration
        </h4>
        <p className="mt-2 text-sm text-slate-500">
          Existing saved sections stay visible even if a plugin is currently
          disabled, so you can inspect and edit them before re-enabling the
          plugin.
        </p>
      </div>

      {configurablePlugins.length === 0 ? (
        <div className="mt-4 rounded-2xl border border-dashed border-slate-200 px-4 py-3 text-sm text-slate-500">
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

function DelegatesPanel({
  spec,
  capabilities,
  toggleDelegate,
}: {
  spec: AgentSpec;
  capabilities: Capabilities | null;
  toggleDelegate: (delegateId: string, checked: boolean) => void;
}) {
  if (!capabilities || capabilities.agents.length === 0) {
    return (
      <div className="rounded-2xl border border-dashed border-slate-200 bg-white p-6 text-sm text-slate-500">
        No other agents are registered yet, so this agent cannot delegate.
      </div>
    );
  }
  return (
    <section className="rounded-2xl border border-slate-200 bg-white p-5 shadow-sm">
      <h3 className="text-lg font-semibold text-slate-950">Delegates</h3>
      <p className="mt-2 text-sm text-slate-500">
        Pick agents this one can hand work off to. The agent itself is omitted
        from the list to prevent obvious self-loops.
      </p>
      <div className="mt-4 grid gap-3 md:grid-cols-2 xl:grid-cols-3">
        {capabilities.agents
          .filter((agentId) => agentId !== spec.id)
          .map((agentId) => (
            <label
              key={agentId}
              className="rounded-xl border border-slate-200 bg-slate-50 px-4 py-3 text-sm text-slate-700"
            >
              <div className="flex items-center gap-3">
                <input
                  type="checkbox"
                  checked={(spec.delegates ?? []).includes(agentId)}
                  onChange={(event) =>
                    toggleDelegate(agentId, event.target.checked)
                  }
                />
                <span className="font-mono text-slate-900">{agentId}</span>
              </div>
            </label>
          ))}
      </div>
    </section>
  );
}

function AdvancedPanel({ spec }: { spec: AgentSpec }) {
  return (
    <section className="rounded-2xl border border-slate-200 bg-white p-5 shadow-sm">
      <h3 className="text-lg font-semibold text-slate-950">JSON Preview</h3>
      <p className="mt-2 text-sm text-slate-500">
        The exact payload that will be PUT to the config API. Useful for sanity
        checking before publish.
      </p>
      <pre className="mt-4 max-h-[36rem] overflow-auto rounded-xl bg-slate-950 p-4 text-xs text-slate-100">
        {JSON.stringify(spec, null, 2)}
      </pre>
    </section>
  );
}
