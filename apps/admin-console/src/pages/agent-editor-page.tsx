import { useEffect, useMemo, useRef, useState } from "react";
import { useQueryClient } from "@tanstack/react-query";
import { useNavigate, useParams, useSearchParams } from "react-router";
import {
  type AgentSpec,
  type ConfigSourceState,
  type RecordMeta,
  configApi,
  deriveSourceState,
} from "@/lib/config-api";
import { AgentPreviewPanel } from "@/components/agent-preview-panel";
import { useToast } from "@/components/toast-provider";
import { useConfirmDialog } from "@/components/confirm-dialog";
import { useUnsavedChangesGuard } from "@/components/unsaved-changes-guard";
import { useTranslation } from "react-i18next";
import {
  AGENT_EDITOR_TABS,
  type AgentEditorTabId,
  readTabFromSearch,
  writeTabToSearch,
} from "@/lib/editor-tabs";
import { pluginConfigEntryKey } from "@/lib/plugin-config";
import { reasoningEffortMode } from "@/lib/reasoning-effort";
import { adminRoutes } from "@/lib/routes";
import { useCapabilitiesQuery } from "@/lib/query/hooks/capabilities";
import { useConfigMetaQuery, useConfigRecordQuery } from "@/lib/query/hooks/config";
import { qk } from "@/lib/query/keys";
import { invalidateConfigMutation } from "@/lib/query/invalidation";
import {
  EMPTY_AGENT,
  diffPatchableFields,
  getOptionalAgentMeta,
  hydrateAgentSpec,
} from "./agent-editor/spec-helpers";
import { EditorSaveBar } from "./agent-editor/editor-save-bar";
import { StickyEditorHeader } from "./agent-editor/sticky-editor-header";
import { BasicsPanel } from "./agent-editor/panels/basics-panel";
import { ToolsPanel } from "./agent-editor/panels/tools-panel";
import { PluginsPanel } from "./agent-editor/panels/plugins-panel";
import { DelegatesPanel } from "./agent-editor/panels/delegates-panel";
import { AdvancedPanel } from "./agent-editor/panels/advanced-panel";
import { HistoryPanel } from "./agent-editor/panels/history-panel";

export function AgentEditorPage() {
  const navigate = useNavigate();
  const { id } = useParams();
  const isNew = id === "new";
  const queryClient = useQueryClient();

  const [searchParams, setSearchParams] = useSearchParams();
  const activeTab = readTabFromSearch(searchParams);
  const setActiveTab = (next: AgentEditorTabId) => {
    setSearchParams(writeTabToSearch(searchParams, next), { replace: true });
  };

  const [spec, setSpec] = useState<AgentSpec>({ ...EMPTY_AGENT });
  const [savedSpec, setSavedSpec] = useState<AgentSpec | null>(null);
  const [originalSpec, setOriginalSpec] = useState<AgentSpec | null>(null);
  const [agentMeta, setAgentMeta] = useState<RecordMeta | null>(null);
  const [saving, setSaving] = useState(false);
  const [activePluginConfig, setActivePluginConfig] = useState<string | null>(null);
  const [historyRefreshKey, setHistoryRefreshKey] = useState(0);
  const [errors, setErrors] = useState<Partial<Record<"id" | "model_id", string>>>({});
  const { t } = useTranslation();
  const toast = useToast();
  const confirmDialog = useConfirmDialog();
  const capabilitiesQuery = useCapabilitiesQuery();
  const agentQuery = useConfigRecordQuery<AgentSpec>("agents", id, {
    enabled: !isNew && Boolean(id),
  });
  const agentMetaQuery = useConfigMetaQuery("agents", id, {
    enabled: !isNew && Boolean(id),
    optional: true,
  });
  const capabilities = capabilitiesQuery.data ?? null;
  const loading = capabilitiesQuery.isPending || (!isNew && agentQuery.isPending);
  const agentError =
    !isNew && agentQuery.error
      ? agentQuery.error instanceof Error
        ? agentQuery.error.message
        : String(agentQuery.error)
      : null;
  const agentMetaError =
    !isNew && agentMetaQuery.error
      ? agentMetaQuery.error instanceof Error
        ? agentMetaQuery.error.message
        : String(agentMetaQuery.error)
      : null;
  const saveDisabled = saving || Boolean(agentMetaError || (!isNew && agentMetaQuery.isPending));
  const initializedAgentIdRef = useRef<string | null>(null);

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

  const sourceState: ConfigSourceState | null = agentMeta ? deriveSourceState(agentMeta) : null;

  useEffect(() => {
    if (capabilitiesQuery.error) {
      toast.error(
        capabilitiesQuery.error instanceof Error
          ? capabilitiesQuery.error.message
          : String(capabilitiesQuery.error),
      );
    }
  }, [capabilitiesQuery.error, toast]);

  useEffect(() => {
    if (agentQuery.error) {
      toast.error(
        agentQuery.error instanceof Error ? agentQuery.error.message : String(agentQuery.error),
      );
    }
  }, [agentQuery.error, toast]);

  useEffect(() => {
    if (agentMetaError) {
      toast.error(`Agent metadata unavailable: ${agentMetaError}`);
    }
  }, [agentMetaError, toast]);

  useEffect(() => {
    if (isNew || !id) {
      initializedAgentIdRef.current = "new";
      return;
    }
    if (!agentQuery.data) return;
    if (initializedAgentIdRef.current === id) return;
    const hydrated = hydrateAgentSpec(agentQuery.data);
    setSpec(hydrated);
    setSavedSpec(hydrated);
    setOriginalSpec(hydrated);
    initializedAgentIdRef.current = id;
  }, [agentQuery.data, id, isNew]);

  useEffect(() => {
    if (isNew || !id) return;
    if (agentMetaQuery.isPending) return;
    setAgentMeta(agentMetaQuery.isError ? null : (agentMetaQuery.data ?? null));
  }, [agentMetaQuery.data, agentMetaQuery.isError, agentMetaQuery.isPending, id, isNew]);

  function validateSpec(current: AgentSpec): typeof errors {
    const next: typeof errors = {};
    if (isNew && !current.id.trim()) {
      next.id = t("validation.required");
    }
    if (!String(current.model_id ?? "").trim()) {
      next.model_id = t("validation.required");
    }
    return next;
  }

  async function handleSave() {
    if (!isNew && agentMetaQuery.isPending) {
      toast.error("Agent metadata is still loading.");
      return;
    }
    if (agentMetaError) {
      toast.error(`Agent metadata unavailable: ${agentMetaError}`);
      return;
    }

    const validationErrors = validateSpec(spec);
    setErrors(validationErrors);
    if (Object.keys(validationErrors).length > 0) {
      toast.error(t("validation.fixErrors"));
      setActiveTab("basics");
      return;
    }
    setSaving(true);
    try {
      const payload = {
        ...spec,
        plugin_ids: [...(spec.plugin_ids ?? [])],
        delegates: [...(spec.delegates ?? [])],
      };

      if (isNew) {
        const created = await configApi.create<typeof payload, AgentSpec>("agents", payload);
        setSavedSpec(created);
        setOriginalSpec(created);
        queryClient.setQueryData(qk.config.get("agents", created.id), created);
        invalidateConfigMutation(queryClient, "agents", created.id);
        toast.success(`Agent "${created.id}" created`);
        navigate(adminRoutes.agent(created.id), { replace: true });
      } else if (sourceState === "builtin" || sourceState === "customized") {
        // For Builtin/Customized records, use PATCH /overrides to preserve
        // upgrade tracking. Only patchable fields are included.
        const patch = diffPatchableFields(spec, originalSpec ?? spec);
        if (Object.keys(patch).length === 0) {
          // Nothing patchable changed; nothing to send.
          toast.success(`Agent "${spec.id}" saved (no patchable changes)`);
        } else {
          await configApi.patchAgentOverrides(spec.id, patch);
          // Refresh spec and meta so the badge updates correctly.
          const [nextSpec, nextMeta] = await Promise.all([
            configApi.get<AgentSpec>("agents", spec.id),
            getOptionalAgentMeta(spec.id),
          ]);
          const hydrated = hydrateAgentSpec(nextSpec);
          setSpec(hydrated);
          setSavedSpec(hydrated);
          setOriginalSpec(hydrated);
          setAgentMeta(nextMeta);
          queryClient.setQueryData(qk.config.get("agents", spec.id), hydrated);
          queryClient.setQueryData(qk.config.meta("agents", spec.id), nextMeta);
          invalidateConfigMutation(queryClient, "agents", spec.id);
          toast.success(`Agent "${spec.id}" saved`);
          setHistoryRefreshKey((k) => k + 1);
        }
      } else {
        const updated = await configApi.update<typeof payload, AgentSpec>(
          "agents",
          spec.id,
          payload,
        );
        setSpec(updated);
        setSavedSpec(updated);
        setOriginalSpec(updated);
        queryClient.setQueryData(qk.config.get("agents", updated.id), updated);
        invalidateConfigMutation(queryClient, "agents", updated.id);
        toast.success(`Agent "${updated.id}" saved`);
        setHistoryRefreshKey((k) => k + 1);
      }
    } catch (saveError) {
      toast.error(saveError instanceof Error ? saveError.message : String(saveError));
    } finally {
      setSaving(false);
    }
  }

  async function handleResetOverrides() {
    if (!id) return;
    const accepted = await confirmDialog({
      title: t("agents.resetOverrides"),
      description: t("agents.resetOverridesConfirm"),
      confirmLabel: t("agents.resetOverrides"),
      tone: "destructive",
    });
    if (!accepted) return;
    try {
      await configApi.clearAgentOverrides(id);
      // Re-fetch spec and meta after reset.
      const [nextSpec, nextMeta] = await Promise.all([
        configApi.get<AgentSpec>("agents", id),
        getOptionalAgentMeta(id),
      ]);
      const hydrated = hydrateAgentSpec(nextSpec);
      setSpec(hydrated);
      setSavedSpec(hydrated);
      setOriginalSpec(hydrated);
      setAgentMeta(nextMeta);
      queryClient.setQueryData(qk.config.get("agents", id), hydrated);
      queryClient.setQueryData(qk.config.meta("agents", id), nextMeta);
      invalidateConfigMutation(queryClient, "agents", id);
      toast.success(`Agent "${id}" overrides cleared`);
    } catch (err) {
      toast.error(err instanceof Error ? err.message : String(err));
    }
  }

  async function handleResetField(field: string) {
    if (!id) return;
    try {
      await configApi.clearAgentOverrideField(id, field);
      const [nextSpec, nextMeta] = await Promise.all([
        configApi.get<AgentSpec>("agents", id),
        getOptionalAgentMeta(id),
      ]);
      const hydrated = hydrateAgentSpec(nextSpec);
      setSpec(hydrated);
      setSavedSpec(hydrated);
      setOriginalSpec(hydrated);
      setAgentMeta(nextMeta);
      queryClient.setQueryData(qk.config.get("agents", id), hydrated);
      queryClient.setQueryData(qk.config.meta("agents", id), nextMeta);
      invalidateConfigMutation(queryClient, "agents", id);
      toast.success(t("agents.resetOverrideFieldDone", { field }));
      setHistoryRefreshKey((k) => k + 1);
    } catch (err) {
      toast.error(err instanceof Error ? err.message : String(err));
    }
  }

  function updateField<K extends keyof AgentSpec>(key: K, value: AgentSpec[K]) {
    setSpec((current) => ({ ...current, [key]: value }));
    if (key === "id" || key === "model_id") {
      setErrors((current) => {
        if (!current[key as "id" | "model_id"]) return current;
        const { [key as "id" | "model_id"]: _removed, ...rest } = current;
        return rest;
      });
    }
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

  const overriddenFields = useMemo(() => {
    const overrides = agentMeta?.user_overrides;
    if (!overrides || typeof overrides !== "object") return new Set<string>();
    return new Set(Object.keys(overrides));
  }, [agentMeta]);
  const isCustomized = sourceState === "customized";

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
        (entry) => pluginConfigEntryKey(entry.plugin.id, entry.schema.key) === activePluginConfig,
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
        <div className="rounded-md border border-line bg-surface p-6 text-sm text-fg-soft shadow-sm">
          Loading agent...
        </div>
      </div>
    );
  }
  if (agentError) {
    return (
      <div className="mx-auto max-w-6xl p-6 md:p-8">
        <div className="rounded-md border border-tone-error/30 bg-tone-error/10 p-4 text-sm text-tone-error">
          Agent unavailable: {agentError}
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
        saveDisabled={saveDisabled}
        onSave={() => void handleSave()}
        activeTab={activeTab}
        onTabChange={setActiveTab}
        sourceState={sourceState}
        onResetOverrides={() => void handleResetOverrides()}
      />

      {agentMetaError && (
        <div className="mx-6 mt-4 rounded-md border border-tone-error/30 bg-tone-error/10 px-4 py-3 text-sm text-tone-error md:mx-8">
          Agent metadata unavailable: {agentMetaError}
        </div>
      )}

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
                  errors={errors}
                  canResetFields={!isNew && isCustomized}
                  overriddenFields={overriddenFields}
                  onResetField={(field) => void handleResetField(field)}
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
              {tab.id === "advanced" && <AdvancedPanel spec={spec} />}
              {tab.id === "history" && (
                <HistoryPanel
                  spec={spec}
                  isNew={isNew}
                  refreshKey={historyRefreshKey}
                  onSpecRestored={(updated) => {
                    setSpec(updated);
                    setSavedSpec(updated);
                    setOriginalSpec(updated);
                    queryClient.setQueryData(qk.config.get("agents", updated.id), updated);
                    invalidateConfigMutation(queryClient, "agents", updated.id);
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
        saveDisabled={saveDisabled}
        spec={spec}
        savedSpec={savedSpec}
        onSave={() => void handleSave()}
      />
    </div>
  );
}
