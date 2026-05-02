import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import {
  Link,
  useNavigate,
  useParams,
  useSearchParams,
} from "react-router";
import { type AgentSpec, type Capabilities, configApi } from "@/lib/config-api";
import { type AuditEvent, type AuditPage, formatActor, summarizeChange } from "@/lib/audit-log";
import { Field } from "@/components/form-components";
import { AgentPreviewPanel } from "@/components/agent-preview-panel";
import { PluginConfigWorkspace } from "@/components/plugin-config-workspace";
import { ToolSelector } from "@/components/tool-selector";
import { useToast } from "@/components/toast-provider";
import { useConfirmDialog } from "@/components/confirm-dialog";
import { useUnsavedChangesGuard } from "@/components/unsaved-changes-guard";
import {
  AGENT_EDITOR_TABS,
  type AgentEditorTabId,
  readTabFromSearch,
  writeTabToSearch,
} from "@/lib/editor-tabs";
import { pluginConfigEntryKey, pluginDisplayName } from "@/lib/plugin-config";
import {
  REASONING_EFFORT_PRESETS,
  reasoningEffortMode,
  reasoningEffortValue,
} from "@/lib/reasoning-effort";
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
  const [historyRefreshKey, setHistoryRefreshKey] = useState(0);
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
        setHistoryRefreshKey((k) => k + 1);
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

  const reasoningMode = reasoningEffortMode(spec.reasoning_effort);

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
        <div className="rounded-2xl border border-line bg-surface p-6 text-sm text-fg-soft shadow-sm">
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
        spec={spec}
        isDirty={isDirty}
        saving={saving}
        onSave={() => void handleSave()}
        activeTab={activeTab}
        onTabChange={setActiveTab}
      />

      <div className="grid gap-6 px-6 py-6 md:px-8 xl:grid-cols-[minmax(0,1fr),24rem]">
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
                />
              )}
              {tab.id === "tools" && (
                <ToolsPanel
                  spec={spec}
                  capabilities={capabilities}
                  updateField={updateField}
                />
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
              {tab.id === "advanced" && <AdvancedPanel spec={spec} />}
              {tab.id === "history" && (
                <HistoryPanel
                  spec={spec}
                  isNew={isNew}
                  refreshKey={historyRefreshKey}
                  onSpecRestored={(updated) => {
                    setSpec(updated);
                    setSavedSpec(updated);
                    setHistoryRefreshKey((k) => k + 1);
                  }}
                />
              )}
            </div>
          ))}
        </div>

        <AgentPreviewPanel draft={spec} />
      </div>

      <EditorSaveBar
        isDirty={isDirty}
        isNew={isNew}
        saving={saving}
        spec={spec}
        onSave={() => void handleSave()}
      />
    </div>
  );
}

function EditorSaveBar({
  isDirty,
  isNew,
  saving,
  spec,
  onSave,
}: {
  isDirty: boolean;
  isNew: boolean;
  saving: boolean;
  spec: AgentSpec;
  onSave: () => void;
}) {
  const toast = useToast();
  const [validating, setValidating] = useState(false);

  if (!isDirty && !isNew) return null;

  async function handleValidate() {
    setValidating(true);
    try {
      await configApi.validateConfig("agents", spec, isNew ? undefined : { id: spec.id });
      toast.success("Validation passed — payload is safe to publish.");
    } catch (err) {
      toast.error(
        `Validation failed: ${err instanceof Error ? err.message : String(err)}`,
      );
    } finally {
      setValidating(false);
    }
  }

  return (
    <div className="sticky bottom-0 z-20 mx-6 mb-4 rounded-md border border-line bg-surface px-4 py-3 shadow-card-lift md:mx-8">
      <div className="flex flex-wrap items-center gap-3">
        <span aria-hidden className="inline-block h-2 w-2 rounded-pill bg-state-progress" />
        <div className="text-sm text-fg">
          {isNew ? (
            <span className="text-fg-strong">Draft — not yet saved.</span>
          ) : (
            <span className="text-fg-strong">Unsaved changes</span>
          )}
          <span className="ml-2 text-fg-soft">
            Save will publish to the runtime config.
          </span>
        </div>
        <div className="ml-auto flex items-center gap-2">
          <button
            type="button"
            onClick={() => void handleValidate()}
            disabled={validating || saving}
            className="inline-flex h-9 items-center rounded-md border border-line-strong bg-surface px-3 text-sm font-medium text-fg transition-colors hover:bg-soft disabled:cursor-not-allowed disabled:opacity-60"
          >
            {validating ? "Validating…" : "Validate"}
          </button>
          <button
            type="button"
            onClick={onSave}
            disabled={saving || validating}
            className="inline-flex h-9 items-center rounded-md bg-fg-strong px-4 text-sm font-medium text-bg transition-colors hover:bg-fg disabled:cursor-not-allowed disabled:opacity-60"
          >
            {saving ? "Saving…" : isNew ? "Save & Publish" : "Save"}
          </button>
        </div>
      </div>
    </div>
  );
}

function StickyEditorHeader({
  isNew,
  agentId,
  spec,
  isDirty,
  saving,
  onSave,
  activeTab,
  onTabChange,
}: {
  isNew: boolean;
  agentId: string;
  spec: AgentSpec;
  isDirty: boolean;
  saving: boolean;
  onSave: () => void;
  activeTab: AgentEditorTabId;
  onTabChange: (next: AgentEditorTabId) => void;
}) {
  const tabRefs = useRef<(HTMLButtonElement | null)[]>([]);

  function handleKeyDown(event: React.KeyboardEvent, index: number) {
    const count = AGENT_EDITOR_TABS.length;
    let nextIndex: number | null = null;

    if (event.key === "ArrowRight") {
      nextIndex = (index + 1) % count;
    } else if (event.key === "ArrowLeft") {
      nextIndex = (index - 1 + count) % count;
    } else if (event.key === "Home") {
      nextIndex = 0;
    } else if (event.key === "End") {
      nextIndex = count - 1;
    }

    if (nextIndex !== null) {
      event.preventDefault();
      const nextTab = AGENT_EDITOR_TABS[nextIndex];
      onTabChange(nextTab.id);
      tabRefs.current[nextIndex]?.focus();
    }
  }

  return (
    <div className="sticky top-0 z-30 border-b border-line bg-surface/95 px-6 pt-6 backdrop-blur md:px-8">
      <div className="flex flex-wrap items-center justify-between gap-4">
        <div className="min-w-0">
          <div className="flex items-center gap-4">
            <Link
              to={adminRoutes.agents}
              className="text-sm font-medium text-fg-soft transition hover:text-fg"
            >
              Back to agents
            </Link>
            {!isNew && agentId && (
              <Link
                to={adminRoutes.auditLogForResource(`agents/${agentId}`)}
                className="text-sm font-medium text-fg-soft transition hover:text-fg"
              >
                History
              </Link>
            )}
          </div>
          <h2 className="mt-2 flex flex-wrap items-center gap-3 text-3xl font-semibold text-fg-strong">
            <span>{isNew ? "New Agent" : `Edit ${agentId}`}</span>
            {isDirty ? (
              <span className="rounded-full bg-tone-warn/15 px-2 py-0.5 text-xs font-medium uppercase tracking-wide text-tone-warn">
                Unsaved changes
              </span>
            ) : !isNew ? (
              <span className="rounded-full bg-tone-success/15 px-2 py-0.5 text-xs font-medium uppercase tracking-wide text-tone-success">
                Up to date
              </span>
            ) : null}
          </h2>
        </div>
        {(isDirty || isNew) ? null : (
          <button
            type="button"
            onClick={onSave}
            disabled={saving}
            className="rounded-xl bg-fg-strong px-4 py-2 text-sm font-medium text-bg transition hover:bg-fg disabled:cursor-not-allowed disabled:opacity-60"
          >
            Save
          </button>
        )}
      </div>

      <div
        role="tablist"
        aria-label="Editor sections"
        aria-orientation="horizontal"
        className="mt-4 flex gap-1 overflow-x-auto"
      >
        {AGENT_EDITOR_TABS.map((tab, index) => {
          const active = tab.id === activeTab;
          const badge = tab.badge?.(spec) ?? null;
          return (
            <button
              key={tab.id}
              ref={(el) => {
                tabRefs.current[index] = el;
              }}
              type="button"
              role="tab"
              id={`tab-${tab.id}`}
              aria-selected={active}
              aria-controls={`panel-${tab.id}`}
              tabIndex={active ? 0 : -1}
              onClick={() => onTabChange(tab.id)}
              onKeyDown={(event) => handleKeyDown(event, index)}
              className={[
                "flex shrink-0 items-center gap-2 rounded-t-lg border-b-2 px-4 py-3 text-sm font-medium transition",
                active
                  ? "border-fg-strong text-fg-strong"
                  : "border-transparent text-fg-soft hover:text-fg",
              ].join(" ")}
            >
              <span>{tab.label}</span>
              {badge && (
                <span
                  aria-hidden
                  className={[
                    "rounded-pill px-1.5 font-mono text-[10px]",
                    active ? "bg-muted text-fg-strong" : "bg-soft text-fg-soft",
                  ].join(" ")}
                >
                  {badge}
                </span>
              )}
            </button>
          );
        })}
      </div>
    </div>
  );
}

function BasicsPanel({
  spec,
  capabilities,
  isNew,
  updateField,
  reasoningMode,
}: {
  spec: AgentSpec;
  capabilities: Capabilities | null;
  isNew: boolean;
  updateField: <K extends keyof AgentSpec>(key: K, value: AgentSpec[K]) => void;
  reasoningMode: ReturnType<typeof reasoningEffortMode>;
}) {
  return (
    <section className="rounded-2xl border border-line bg-surface p-5 shadow-sm">
      <h3 className="text-lg font-semibold text-fg-strong">Basics</h3>
      <div className="mt-4 grid gap-4 md:grid-cols-2">
        <Field label="Agent ID">
          <input
            type="text"
            value={spec.id}
            disabled={!isNew}
            onChange={(event) => updateField("id", event.target.value)}
            className="w-full rounded-xl border border-line-strong px-3 py-2 text-sm text-fg-strong outline-none transition focus:border-line-strong disabled:bg-muted disabled:text-fg-soft"
          />
        </Field>
        <Field label="Model">
          <select
            value={String(spec.model_id ?? "")}
            onChange={(event) => updateField("model_id", event.target.value)}
            className="w-full rounded-xl border border-line-strong bg-surface px-3 py-2 text-sm text-fg-strong outline-none transition focus:border-line-strong"
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
            className="w-full rounded-xl border border-line-strong px-3 py-2 text-sm text-fg-strong outline-none transition focus:border-line-strong"
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
            className="w-full rounded-xl border border-line-strong px-3 py-2 text-sm text-fg-strong outline-none transition focus:border-line-strong"
          />
        </Field>
        <Field label="Reasoning effort">
          <div className="flex flex-wrap items-center gap-2">
            <select
              value={
                reasoningMode.kind === "default"
                  ? "__default__"
                  : reasoningMode.kind === "preset"
                    ? reasoningMode.value
                    : "__custom__"
              }
              onChange={(event) => {
                const choice = event.target.value;
                if (choice === "__default__") {
                  updateField(
                    "reasoning_effort",
                    reasoningEffortValue({ kind: "default" }) as
                      | string
                      | number
                      | null
                      | undefined,
                  );
                  return;
                }
                if (choice === "__custom__") {
                  updateField(
                    "reasoning_effort",
                    reasoningEffortValue({
                      kind: "custom",
                      value:
                        reasoningMode.kind === "custom"
                          ? reasoningMode.value
                          : "",
                    }) as string | number | null | undefined,
                  );
                  return;
                }
                updateField(
                  "reasoning_effort",
                  reasoningEffortValue({
                    kind: "preset",
                    value: choice as (typeof REASONING_EFFORT_PRESETS)[number],
                  }) as string | number | null | undefined,
                );
              }}
              className="rounded-xl border border-line-strong bg-surface px-3 py-2 text-sm text-fg-strong outline-none transition focus:border-line-strong"
            >
              <option value="__default__">Provider default</option>
              {REASONING_EFFORT_PRESETS.map((preset) => (
                <option key={preset} value={preset}>
                  {preset}
                </option>
              ))}
              <option value="__custom__">Custom…</option>
            </select>
            {reasoningMode.kind === "custom" ? (
              <input
                type="text"
                value={reasoningMode.value}
                onChange={(event) =>
                  updateField(
                    "reasoning_effort",
                    reasoningEffortValue({
                      kind: "custom",
                      value: event.target.value,
                    }) as string | number | null | undefined,
                  )
                }
                placeholder="e.g. 1, 2, ultra"
                className="w-32 rounded-xl border border-line-strong px-3 py-2 text-sm text-fg-strong outline-none transition focus:border-line-strong"
              />
            ) : null}
          </div>
        </Field>
      </div>

      <div className="mt-4">
        <Field label="System prompt">
          <textarea
            value={String(spec.system_prompt ?? "")}
            onChange={(event) => updateField("system_prompt", event.target.value)}
            rows={8}
            className="w-full rounded-xl border border-line-strong px-3 py-2 font-mono text-sm text-fg-strong outline-none transition focus:border-line-strong"
          />
        </Field>
        <p className="mt-1 text-xs text-fg-soft">
          {String(spec.system_prompt ?? "").length} characters
        </p>
      </div>
    </section>
  );
}

function ToolsPanel({
  spec,
  capabilities,
  updateField,
}: {
  spec: AgentSpec;
  capabilities: Capabilities | null;
  updateField: <K extends keyof AgentSpec>(key: K, value: AgentSpec[K]) => void;
}) {
  if (!capabilities || capabilities.tools.length === 0) {
    return (
      <div className="rounded-2xl border border-dashed border-line bg-surface p-6 text-sm text-fg-soft">
        No tools are currently published. Once plugins or MCP servers register
        tools, they will appear here.
      </div>
    );
  }
  return (
    <>
      <ToolSelector
        title="Allowed Tools"
        description='"All tools" is the default — every published tool is exposed. Switch to Custom to restrict the agent to a specific subset.'
        tools={capabilities.tools}
        value={spec.allowed_tools}
        onChange={(next) => updateField("allowed_tools", next)}
        variant="include"
      />
      <ToolSelector
        title="Excluded Tools"
        description="Excluded tools are removed from the effective allow-list, even if they appear in 'All tools'. Useful for keeping a tool published to other agents but blocking it here."
        tools={capabilities.tools}
        value={spec.excluded_tools}
        onChange={(next) => updateField("excluded_tools", next)}
        variant="exclude"
      />
    </>
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
      <div className="rounded-2xl border border-dashed border-line bg-surface p-6 text-sm text-fg-soft">
        No plugins are currently registered.
      </div>
    );
  }
  return (
    <section className="rounded-2xl border border-line bg-surface p-5 shadow-sm">
      <h3 className="text-lg font-semibold text-fg-strong">Plugins</h3>
      <p className="mt-2 text-sm text-fg-soft">
        Enable agent plugins here. Plugins with agent-level settings expose
        their configuration forms below.
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
                  <div className="font-mono text-fg-strong">
                    {pluginDisplayName(plugin.id)}
                  </div>
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
        <h4 className="text-base font-semibold text-fg-strong">
          Plugin Configuration
        </h4>
        <p className="mt-2 text-sm text-fg-soft">
          Existing saved sections stay visible even if a plugin is currently
          disabled, so you can inspect and edit them before re-enabling the
          plugin.
        </p>
      </div>

      {configurablePlugins.length === 0 ? (
        <div className="mt-4 rounded-2xl border border-dashed border-line px-4 py-3 text-sm text-fg-soft">
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
      <div className="rounded-2xl border border-dashed border-line bg-surface p-6 text-sm text-fg-soft">
        No other agents are registered yet, so this agent cannot delegate.
      </div>
    );
  }
  const selected = spec.delegates ?? [];
  return (
    <section className="rounded-2xl border border-line bg-surface p-5 shadow-sm">
      <div className="flex flex-wrap items-end justify-between gap-3">
        <div>
          <h3 className="text-lg font-semibold text-fg-strong">Delegates</h3>
          <p className="mt-2 max-w-xl text-sm text-fg-soft">
            Pick agents this one can hand work off to. Self-loops are blocked
            statically; longer cycles (A → B → A) are detected at runtime by the
            scheduler.
          </p>
        </div>
        {selected.length > 0 && (
          <div className="flex flex-wrap items-center gap-1.5 text-xs text-fg-soft">
            <span className="text-fg-faint">selected</span>
            {selected.map((id) => (
              <span
                key={id}
                className="inline-flex items-center gap-1 rounded-pill border border-agent-stripe/30 bg-agent-tint px-2 py-0.5 font-mono text-agent-fg"
              >
                {id}
              </span>
            ))}
          </div>
        )}
      </div>
      <div className="mt-4 grid gap-3 md:grid-cols-2 xl:grid-cols-3">
        {capabilities.agents
          .filter((agentId) => agentId !== spec.id)
          .map((agentId) => {
            const checked = selected.includes(agentId);
            return (
              <label
                key={agentId}
                className={[
                  "rounded-xl border px-4 py-3 text-sm transition-colors",
                  checked
                    ? "border-agent-stripe/40 bg-agent-tint text-agent-fg"
                    : "border-line bg-soft text-fg hover:border-line-strong",
                ].join(" ")}
              >
                <div className="flex items-center gap-3">
                  <input
                    type="checkbox"
                    checked={checked}
                    onChange={(event) =>
                      toggleDelegate(agentId, event.target.checked)
                    }
                  />
                  <span className="font-mono text-fg-strong">{agentId}</span>
                </div>
              </label>
            );
          })}
      </div>
    </section>
  );
}

function AdvancedPanel({ spec }: { spec: AgentSpec }) {
  return (
    <section className="rounded-2xl border border-line bg-surface p-5 shadow-sm">
      <h3 className="text-lg font-semibold text-fg-strong">JSON Preview</h3>
      <p className="mt-2 text-sm text-fg-soft">
        The exact payload that will be PUT to the config API. Useful for sanity
        checking before publish.
      </p>
      <pre className="mt-4 max-h-[36rem] overflow-auto rounded-xl bg-fg-strong p-4 text-xs text-bg">
        {JSON.stringify(spec, null, 2)}
      </pre>
    </section>
  );
}

const ACTION_BADGE: Record<string, string> = {
  create: "bg-tone-success/15 text-tone-success",
  update: "bg-blue-100 text-blue-800",
  delete: "bg-tone-error/15 text-tone-error",
  restart: "bg-tone-warn/15 text-tone-warn",
  publish: "bg-violet-100 text-violet-800",
  restore: "bg-purple-100 text-purple-800",
};

function HistoryPanel({
  spec,
  isNew,
  refreshKey,
  onSpecRestored,
}: {
  spec: AgentSpec;
  isNew: boolean;
  refreshKey: number;
  onSpecRestored: (updated: AgentSpec) => void;
}) {
  const toast = useToast();
  const confirm = useConfirmDialog();
  const [page, setPage] = useState<AuditPage | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [selectedEvent, setSelectedEvent] = useState<AuditEvent | null>(null);
  const [restoring, setRestoring] = useState<string | null>(null);

  const load = useCallback(async () => {
    if (isNew || !spec.id) return;
    setLoading(true);
    setError(null);
    try {
      const result = await configApi.auditLog({ resource: `agents/${spec.id}`, limit: 50 });
      setPage(result);
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setLoading(false);
    }
  }, [isNew, spec.id]);

  useEffect(() => {
    void load();
    // refreshKey is intentionally included: bumping it triggers a re-fetch
    // after a successful save or restore without causing a re-render loop.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [load, refreshKey]);

  async function handleRestore(event: AuditEvent) {
    const targetSpec = event.action === "delete" ? event.before : event.after;
    const confirmed = await confirm({
      title: "Restore agent to this version?",
      description: (
        <div className="space-y-3">
          <p className="text-xs text-fg-soft">
            Restoring will overwrite the current agent configuration with the version from this event.
          </p>
          <div className="grid grid-cols-2 gap-3">
            <div>
              <p className="mb-1 text-xs font-medium uppercase tracking-wide text-fg-soft">Current</p>
              <pre className="max-h-48 overflow-auto rounded-xl border border-line bg-soft p-2 text-xs text-fg">
                {JSON.stringify(spec, null, 2)}
              </pre>
            </div>
            <div>
              <p className="mb-1 text-xs font-medium uppercase tracking-wide text-fg-soft">This version</p>
              <pre className="max-h-48 overflow-auto rounded-xl border border-line bg-soft p-2 text-xs text-fg">
                {targetSpec != null ? JSON.stringify(targetSpec, null, 2) : "—"}
              </pre>
            </div>
          </div>
        </div>
      ),
      confirmLabel: "Restore",
      tone: "destructive",
    });

    if (!confirmed) return;

    setRestoring(event.id);
    try {
      await configApi.restoreConfig("agents", spec.id, event.id);
      const shortId = event.id.slice(0, 8);
      toast.success(`Agent restored to version ${shortId}`);
      const refreshed = await configApi.get<AgentSpec>("agents", spec.id);
      const hydrated: AgentSpec = { sections: {}, plugin_ids: [], delegates: [], ...refreshed };
      onSpecRestored(hydrated);
      void load();
    } catch (err) {
      toast.error(err instanceof Error ? err.message : String(err));
    } finally {
      setRestoring(null);
    }
  }

  if (isNew || !spec.id) {
    return (
      <section className="rounded-2xl border border-dashed border-line bg-surface p-6 text-center text-sm text-fg-soft shadow-sm">
        Save the agent first to see its history.
      </section>
    );
  }

  return (
    <section className="rounded-2xl border border-line bg-surface shadow-sm">
      <div className="flex items-center justify-between border-b border-line px-5 py-4">
        <h3 className="text-lg font-semibold text-fg-strong">History</h3>
        <button
          type="button"
          onClick={() => void load()}
          disabled={loading}
          className="text-xs font-medium text-fg-soft transition hover:text-fg-strong disabled:opacity-60"
        >
          {loading ? "Loading…" : "Refresh"}
        </button>
      </div>

      {error && (
        <div className="px-5 py-3 text-sm text-tone-error">{error}</div>
      )}

      {!error && page && (
        <table className="min-w-full text-sm">
          <thead className="bg-soft text-left text-xs uppercase tracking-wide text-fg-soft">
            <tr>
              <th className="px-4 py-3">Time</th>
              <th className="px-4 py-3">Actor</th>
              <th className="px-4 py-3">Action</th>
              <th className="px-4 py-3">Change</th>
              <th className="px-4 py-3"></th>
            </tr>
          </thead>
          <tbody className="divide-y divide-line">
            {page.items.length === 0 ? (
              <tr>
                <td colSpan={5} className="px-4 py-8 text-center text-sm text-fg-soft">
                  No history yet.
                </td>
              </tr>
            ) : (
              page.items.map((event) => {
                const actor = formatActor(event.actor);
                const ts = new Date(event.ts);
                return (
                  <tr key={event.id} className="hover:bg-soft">
                    <td className="px-4 py-3 font-mono text-xs text-fg">
                      {ts.toLocaleString()}
                    </td>
                    <td className="px-4 py-3 text-sm text-fg">
                      <span className="font-mono text-xs">{actor.hash}</span>
                      {actor.label && (
                        <span className="ml-1 text-fg-soft">/{actor.label}</span>
                      )}
                    </td>
                    <td className="px-4 py-3">
                      <span
                        className={[
                          "inline-flex items-center rounded-full px-2.5 py-0.5 text-xs font-medium",
                          ACTION_BADGE[event.action] ?? "bg-muted text-fg",
                        ].join(" ")}
                      >
                        {event.action}
                      </span>
                    </td>
                    <td className="max-w-xs truncate px-4 py-3 text-xs text-fg-soft">
                      {summarizeChange(event)}
                    </td>
                    <td className="px-4 py-3">
                      <div className="flex items-center gap-3">
                        <button
                          type="button"
                          onClick={() => setSelectedEvent(event)}
                          className="text-xs font-medium text-fg-soft transition hover:text-fg-strong"
                        >
                          View
                        </button>
                        {event.action !== "restart" && (
                          <button
                            type="button"
                            onClick={() => void handleRestore(event)}
                            disabled={restoring === event.id}
                            className="text-xs font-medium text-tone-error transition hover:text-tone-error disabled:opacity-60"
                          >
                            {restoring === event.id ? "Restoring…" : "Restore"}
                          </button>
                        )}
                      </div>
                    </td>
                  </tr>
                );
              })
            )}
          </tbody>
        </table>
      )}

      {selectedEvent && (
        <HistoryEventPanel event={selectedEvent} onClose={() => setSelectedEvent(null)} />
      )}
    </section>
  );
}

function HistoryEventPanel({
  event,
  onClose,
}: {
  event: AuditEvent;
  onClose: () => void;
}) {
  const actor = formatActor(event.actor);
  return (
    <div
      role="dialog"
      aria-modal="true"
      aria-label="Audit event details"
      className="fixed inset-0 z-50 flex items-start justify-end bg-black/30"
      onClick={(e) => {
        if (e.target === e.currentTarget) onClose();
      }}
    >
      <div className="flex h-full w-full max-w-2xl flex-col overflow-y-auto bg-surface shadow-2xl md:max-w-xl">
        <div className="flex items-center justify-between border-b border-line px-6 py-4">
          <h3 className="text-base font-semibold text-fg-strong">Audit event</h3>
          <button
            type="button"
            onClick={onClose}
            className="rounded-md px-2 py-1 text-sm text-fg-soft hover:bg-muted"
          >
            Close
          </button>
        </div>

        <dl className="grid gap-3 px-6 py-4 text-sm">
          <div className="flex items-baseline gap-3">
            <dt className="w-24 shrink-0 text-xs font-medium text-fg-soft">ID</dt>
            <dd className="min-w-0 font-mono text-xs text-fg-strong">{event.id}</dd>
          </div>
          <div className="flex items-baseline gap-3">
            <dt className="w-24 shrink-0 text-xs font-medium text-fg-soft">Time</dt>
            <dd className="min-w-0 font-mono text-xs text-fg-strong">{event.ts}</dd>
          </div>
          <div className="flex items-baseline gap-3">
            <dt className="w-24 shrink-0 text-xs font-medium text-fg-soft">Actor</dt>
            <dd className="min-w-0 text-fg-strong">
              <span className="font-mono text-xs">{actor.hash}</span>
              {actor.label && <span className="ml-1 text-fg-soft">/{actor.label}</span>}
            </dd>
          </div>
          <div className="flex items-baseline gap-3">
            <dt className="w-24 shrink-0 text-xs font-medium text-fg-soft">Action</dt>
            <dd className="min-w-0">
              <span
                className={[
                  "inline-flex items-center rounded-full px-2.5 py-0.5 text-xs font-medium",
                  ACTION_BADGE[event.action] ?? "bg-muted text-fg",
                ].join(" ")}
              >
                {event.action}
              </span>
            </dd>
          </div>
        </dl>

        <div className="grid gap-4 px-6 pb-6 md:grid-cols-2">
          <div>
            <p className="mb-2 text-xs font-medium uppercase tracking-wide text-fg-soft">Before</p>
            <pre className="overflow-auto rounded-xl border border-line bg-soft p-3 text-xs leading-relaxed text-fg">
              {event.before != null ? JSON.stringify(event.before, null, 2) : "—"}
            </pre>
          </div>
          <div>
            <p className="mb-2 text-xs font-medium uppercase tracking-wide text-fg-soft">After</p>
            <pre className="overflow-auto rounded-xl border border-line bg-soft p-3 text-xs leading-relaxed text-fg">
              {event.after != null ? JSON.stringify(event.after, null, 2) : "—"}
            </pre>
          </div>
        </div>
      </div>
    </div>
  );
}
