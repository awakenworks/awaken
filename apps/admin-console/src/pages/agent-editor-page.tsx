import { useEffect, useState } from "react";
import { Link, useNavigate, useParams } from "react-router";
import { type AgentSpec, type Capabilities, configApi } from "@/lib/config-api";
import { Field } from "@/components/form-components";
import { AgentPreviewPanel } from "@/components/agent-preview-panel";
import { PluginConfigWorkspace } from "@/components/plugin-config-workspace";
import { pluginConfigEntryKey, pluginDisplayName } from "@/lib/plugin-config";
import { adminRoutes } from "@/lib/routes";

const EMPTY_AGENT: AgentSpec = {
  id: "",
  model: "",
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

  const [spec, setSpec] = useState<AgentSpec>({ ...EMPTY_AGENT });
  const [capabilities, setCapabilities] = useState<Capabilities | null>(null);
  const [loading, setLoading] = useState(!isNew);
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [success, setSuccess] = useState<string | null>(null);
  const [activePluginConfig, setActivePluginConfig] = useState<string | null>(null);

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
          setError(
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
          setSpec({
            sections: {},
            plugin_ids: [],
            delegates: [],
            ...nextSpec,
          });
          setError(null);
        }
      } catch (loadError) {
        if (!cancelled) {
          setError(loadError instanceof Error ? loadError.message : String(loadError));
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
    setSuccess(null);
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
        setSuccess("Agent created.");
        navigate(adminRoutes.agent(created.id), { replace: true });
      } else {
        const updated = await configApi.update<typeof payload, AgentSpec>(
          "agents",
          spec.id,
          payload,
        );
        setSpec(updated);
        setSuccess("Agent saved.");
      }
      setError(null);
    } catch (saveError) {
      setError(saveError instanceof Error ? saveError.message : String(saveError));
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
    const currentAllowed = spec.allowed_tools;

    if (checked) {
      // Re-allow: add toolId back to the allowed list.
      if (!currentAllowed) {
        return; // already allowing all tools
      }
      const nextAllowed = [...currentAllowed, toolId];
      updateField(
        "allowed_tools",
        nextAllowed.length >= allToolIds.length ? undefined : nextAllowed,
      );
      return;
    }

    // Disallow: remove toolId from the allowed list.
    const baseAllowed =
      currentAllowed && currentAllowed.length > 0 ? currentAllowed : allToolIds;
    updateField(
      "allowed_tools",
      baseAllowed.filter((value) => value !== toolId),
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

    if (
      !activePluginConfig ||
      !visiblePluginSchemas.some(
        (entry) =>
          pluginConfigEntryKey(entry.plugin.id, entry.schema.key) === activePluginConfig,
      )
    ) {
      setActivePluginConfig(
        pluginConfigEntryKey(
          visiblePluginSchemas[0].plugin.id,
          visiblePluginSchemas[0].schema.key,
        ),
      );
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
    <div className="mx-auto max-w-[96rem] p-6 md:p-8">
      <div className="mb-6 flex items-center justify-between gap-4">
        <div>
          <Link
            to={adminRoutes.agents}
            className="text-sm font-medium text-slate-500 transition hover:text-slate-700"
          >
            Back to agents
          </Link>
          <h2 className="mt-2 text-3xl font-semibold text-slate-950">
            {isNew ? "New Agent" : `Edit ${spec.id}`}
          </h2>
        </div>
        <button
          type="button"
          onClick={() => void handleSave()}
          disabled={saving}
          className="rounded-xl bg-slate-950 px-4 py-2 text-sm font-medium text-white transition hover:bg-slate-800 disabled:cursor-not-allowed disabled:opacity-60"
        >
          {saving ? "Saving..." : "Save"}
        </button>
      </div>

      {error ? (
        <div className="mb-4 rounded-2xl border border-rose-200 bg-rose-50 px-4 py-3 text-sm text-rose-700">
          {error}
        </div>
      ) : null}

      {success ? (
        <div className="mb-4 rounded-2xl border border-emerald-200 bg-emerald-50 px-4 py-3 text-sm text-emerald-700">
          {success}
        </div>
      ) : null}

      <div className="grid gap-6 xl:grid-cols-[minmax(0,1fr),24rem]">
        <div>
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
              value={String(spec.model ?? "")}
              onChange={(event) => updateField("model", event.target.value)}
              className="w-full rounded-xl border border-slate-300 bg-white px-3 py-2 text-sm text-slate-900 outline-none transition focus:border-slate-500"
            >
              <option value="">Select a model</option>
              {(capabilities?.models ?? []).map((model) => (
                <option key={model.id} value={model.id}>
                  {model.id} ({model.model})
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
              onChange={(event) =>
                updateField("system_prompt", event.target.value)
              }
              rows={8}
              className="w-full rounded-xl border border-slate-300 px-3 py-2 font-mono text-sm text-slate-900 outline-none transition focus:border-slate-500"
            />
          </Field>
        </div>
      </section>

      {capabilities && capabilities.tools.length > 0 ? (
        <section className="mt-6 rounded-2xl border border-slate-200 bg-white p-5 shadow-sm">
          <h3 className="text-lg font-semibold text-slate-950">Allowed Tools</h3>
          <p className="mt-2 text-sm text-slate-500">
            Leaving every tool selected keeps the default runtime behavior.
          </p>
          <div className="mt-4 grid gap-3 md:grid-cols-2 xl:grid-cols-3">
            {capabilities.tools.map((tool) => {
              const allowed = spec.allowed_tools;
              const checked =
                !allowed || allowed.length === 0 || allowed.includes(tool.id);
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
      ) : null}

      {capabilities && capabilities.plugins.length > 0 ? (
        <section className="mt-6 rounded-2xl border border-slate-200 bg-white p-5 shadow-sm">
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
      ) : null}

      {capabilities && capabilities.agents.length > 0 ? (
        <section className="mt-6 rounded-2xl border border-slate-200 bg-white p-5 shadow-sm">
          <h3 className="text-lg font-semibold text-slate-950">Delegates</h3>
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
      ) : null}

      <section className="mt-6 rounded-2xl border border-slate-200 bg-white p-5 shadow-sm">
        <h3 className="text-lg font-semibold text-slate-950">JSON Preview</h3>
        <pre className="mt-4 max-h-[28rem] overflow-auto rounded-xl bg-slate-950 p-4 text-xs text-slate-100">
          {JSON.stringify(spec, null, 2)}
        </pre>
      </section>
        </div>

        <AgentPreviewPanel draft={spec} />
      </div>
    </div>
  );
}
