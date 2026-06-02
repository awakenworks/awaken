import { useEffect, useMemo, useRef, useState } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { Link, useNavigate, useParams, useSearchParams } from "react-router";
import {
  type AgentSpec,
  type Capabilities,
  type ConfigSourceState,
  type ConfigMetaItem,
  type McpServerRecord,
  type PermissionPreviewResponse,
  type RecordMeta,
  ConfigApiError,
  capabilitiesFromResult,
  configApi,
  deriveSourceState,
} from "@/lib/config-api";
import { type AuditEvent, formatActor, summarizeChange } from "@/lib/audit-log";
import { AgentPreviewPanel } from "@/components/agent-preview-panel";
import { AdminAssistantLockedToolsSection } from "@/components/admin-assistant-locked-tools-section";
import { AgentFrontendIntegrationCard } from "@/components/agent-frontend-integration-card";
import { EditorSourceBadge } from "./agent-editor/editor-source-badge";
import {
  AWAKEN_BACKEND_KIND,
  BasicsPanel,
  applyBackendConfig,
  currentBackend,
  syncAwakenBackend,
} from "./agent-editor/panels/basics-panel";
import { ToolsPanel as ToolSelectorsPanel } from "./agent-editor/panels/tools-panel";
import { VisibleToolDescriptors } from "./agent-editor/panels/tool-descriptors";
import { BackendConfigSection } from "./agent-editor/panels/backend-config-section";
import { CompactionSection } from "./agent-editor/panels/compaction-section";
import { ContextPolicySection } from "./agent-editor/panels/context-policy-section";
import { DelegatesPanel } from "./agent-editor/panels/delegates-panel";
import { McpServersPanel } from "./agent-editor/panels/mcp-servers-panel";
import { PluginsPanel } from "./agent-editor/panels/plugins-panel";
import { RemoteA2aAgentReadOnlyPage } from "./agent-editor/remote-a2a-readonly-page";
import { RemoteEndpointReadonlySection } from "./agent-editor/panels/remote-endpoint-readonly-section";
import { SkillsPanel } from "./agent-editor/panels/skills-panel";
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
import { adminRoutes } from "@/lib/routes";
import { a2aServerIdForAgent } from "@/lib/a2a-agent";
import { useCapabilitiesQuery } from "@/lib/query/hooks/capabilities";
import {
  useConfigListQuery,
  useConfigMetaListQuery,
  useConfigMetaQuery,
  useConfigRecordQuery,
} from "@/lib/query/hooks/config";
import { useAuditLogInfiniteQuery } from "@/lib/query/hooks/audit";
import { qk } from "@/lib/query/keys";
import { invalidateConfigMutation } from "@/lib/query/invalidation";
import {
  canonicalStringify,
  changedRedactionMarkerPaths,
  cloneAgentSpecForEditor,
  computeRedactedDiff,
  deepEqualCanonical,
  diffPatchableAgentFields,
  applyPluginSectionDefaults,
  lockedFieldChange,
  mergeLockedFields,
  redactAgentSpecForEditing,
  redactAgentSpecForDisplay,
  redactSecretString,
  redactSecretsForDisplay,
  restoreUnchangedRedactions,
  togglePluginState,
  unknownAgentSpecFields,
} from "@/lib/agent-editor-helpers";
import { deriveAllowedMode, isToolAllowed, type AgentSpecCatalog } from "@/lib/tool-catalog";
import { safeErrorMessage } from "@/lib/safe-error-message";
const EMPTY_AGENT: AgentSpec = {
  id: "",
  backend: {
    kind: "awaken",
    version: 1,
    config: {
      model_id: "",
      system_prompt: "",
      max_rounds: 16,
    },
  },
  model_id: "",
  system_prompt: "",
  max_rounds: 16,
  max_continuation_retries: 2,
  plugin_ids: [],
  sections: {},
  delegates: [],
};

function pluginConfigWarnings(
  pluginId: string,
  selected: boolean,
  hasStoredConfig: boolean,
  activeHookFilter: string[] | undefined,
): string[] {
  const warnings: string[] = [];
  if (hasStoredConfig && !selected) {
    warnings.push(
      `This configuration is saved but inactive because plugin \`${pluginId}\` is disabled.`,
    );
  }
  if (
    selected &&
    activeHookFilter &&
    activeHookFilter.length > 0 &&
    !activeHookFilter.includes(pluginId)
  ) {
    warnings.push(
      `Plugin \`${pluginId}\` is enabled but excluded by active_hook_filter, so its hooks will not run.`,
    );
  }
  return warnings;
}
async function getOptionalAgentMeta(id: string): Promise<RecordMeta | null> {
  try {
    return await configApi.getMeta("agents", id);
  } catch (error) {
    if (error instanceof ConfigApiError && error.status === 404) {
      return null;
    }
    throw error;
  }
}
function hydrateAgentSpec(spec: AgentSpec): AgentSpec {
  return {
    sections: {},
    plugin_ids: [],
    delegates: [],
    ...spec,
  };
}
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
  const mcpServersQuery = useConfigListQuery<McpServerRecord>("mcp-servers", {
    enabled: activeTab === "tools",
  });
  const toolMetaQuery = useConfigMetaListQuery("tools", {
    enabled: activeTab === "tools",
  });
  const agentQuery = useConfigRecordQuery<AgentSpec>("agents", id, {
    enabled: !isNew && Boolean(id),
  });
  const agentMetaQuery = useConfigMetaQuery("agents", id, {
    enabled: !isNew && Boolean(id),
    optional: true,
  });
  const capabilities = capabilitiesFromResult(capabilitiesQuery.data);
  const loading = capabilitiesQuery.isPending || (!isNew && agentQuery.isPending);
  const agentError = !isNew && agentQuery.error ? safeErrorMessage(agentQuery.error) : null;
  const mcpServersError = mcpServersQuery.error ? safeErrorMessage(mcpServersQuery.error) : null;
  const toolMetaError = toolMetaQuery.error ? safeErrorMessage(toolMetaQuery.error) : null;
  const agentMetaError =
    !isNew && agentMetaQuery.error ? safeErrorMessage(agentMetaQuery.error) : null;
  const saveDisabled = saving || Boolean(agentMetaError || (!isNew && agentMetaQuery.isPending));
  const initializedAgentIdRef = useRef<string | null>(null);
  const isDirty = useMemo(() => {
    if (saving) return false;
    if (isNew) {
      // Compare the whole draft against the empty baseline. Earlier this
      // branch only checked id / system_prompt / model_id / plugin_ids.length,
      // so users could lose unsaved edits in context_policy, allowed/excluded
      // tools, delegates, reasoning_effort, sections, or Raw JSON without
      // triggering the unsaved-changes guard.
      return !deepEqualCanonical(spec, EMPTY_AGENT);
    }
    if (!savedSpec) return false;
    return !deepEqualCanonical(spec, savedSpec);
  }, [spec, savedSpec, isNew, saving]);

  useUnsavedChangesGuard({ enabled: isDirty });

  const sourceState: ConfigSourceState | null = agentMeta ? deriveSourceState(agentMeta) : null;
  const a2aServerId = a2aServerIdForAgent(spec);
  const isRemoteA2aAgent =
    !isNew && (spec.endpoint?.backend === "a2a" || Boolean(spec.registry));

  useEffect(() => {
    if (capabilitiesQuery.error) {
      toast.error(safeErrorMessage(capabilitiesQuery.error));
    }
  }, [capabilitiesQuery.error, toast]);

  useEffect(() => {
    if (agentQuery.error) {
      toast.error(safeErrorMessage(agentQuery.error));
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
    if (currentBackend(current).kind === AWAKEN_BACKEND_KIND && !String(current.model_id ?? "").trim()) {
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
    // Track whether the in-flight save targets a builtin / customized
    // record via `PATCH /overrides`. On failure the catch block
    // refetches the agent so the editor's draft matches what the server
    // actually holds. The PATCH itself is transactional (a single body
    // carries upserts plus `_clear`; the server applies both under one
    // `apply_locked` guard and rolls back on any sub-step error), so a
    // partial-write scenario doesn't happen here — but the refetch is
    // still worth doing because the error may come from validation,
    // optimistic-concurrency failure, or stale client state, any of
    // which leave the user looking at a draft that doesn't match the
    // server's view.
    let customizedSaveInFlight = false;
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
        customizedSaveInFlight = true;
        // For Builtin/Customized records, use PATCH /overrides to preserve
        // upgrade tracking. Only patchable fields are included.
        const plan = diffPatchableAgentFields(spec, originalSpec ?? spec);
        const hasUpserts = Object.keys(plan.patch).length > 0;
        const hasClears = plan.clear.length > 0;
        if (!hasUpserts && !hasClears) {
          // Nothing patchable changed; nothing to send.
          toast.success(`Agent "${spec.id}" saved (no patchable changes)`);
        } else {
          // R11 #3 — Combine upserts + clears into a single transactional
          // PATCH. The server applies both inside one `apply_locked`
          // guard and emits a single audit event; a failure leaves the
          // record untouched. Previously the client issued one PATCH
          // followed by N DELETE calls, which could leave the agent in
          // a partial state if any DELETE failed.
          const body: Record<string, unknown> = { ...plan.patch };
          if (hasClears) {
            body._clear = plan.clear.map((field) => String(field));
          }
          await configApi.patchAgentOverrides(spec.id, body);
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
      toast.error(safeErrorMessage(saveError));
      // Customized PATCH is now transactional (server applies upserts +
      // `_clear` together under one `apply_locked` guard), so on failure
      // the server itself is in a consistent state. The refetch here
      // exists to re-sync the EDITOR's view: the error may come from
      // optimistic-concurrency rejection, validation, or stale
      // `originalSpec`, all of which leave the local draft diverged
      // from the server's actual current state.
      if (customizedSaveInFlight && !isNew && id) {
        try {
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
        } catch (refetchError) {
          // Refetch itself failed (e.g. network down). The original
          // saveError is the actionable one; surface the refetch
          // failure as a secondary toast so the user knows the UI may
          // also be stale.
          toast.error(
            `Could not refresh agent state after save error: ${safeErrorMessage(refetchError)}`,
          );
        }
      }
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
      toast.error(safeErrorMessage(err));
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
      toast.error(safeErrorMessage(err));
    }
  }

  function updateField<K extends keyof AgentSpec>(key: K, value: AgentSpec[K]) {
    setSpec((current) => syncAwakenBackend({ ...current, [key]: value }));
    if (key === "id" || key === "model_id") {
      setErrors((current) => {
        if (!current[key as "id" | "model_id"]) return current;
        const { [key as "id" | "model_id"]: _removed, ...rest } = current;
        return rest;
      });
    }
  }

  function updateBackend(kind: string, config: Record<string, unknown>) {
    setSpec((current) => {
      const backendInfo = capabilities?.backends?.find((candidate) => candidate.kind === kind);
      const version = backendInfo?.version ?? current.backend?.version ?? 1;
      return applyBackendConfig(current, kind, version, config);
    });
    if (kind !== AWAKEN_BACKEND_KIND) {
      setErrors((current) => {
        if (!current.model_id) return current;
        const { model_id: _removed, ...rest } = current;
        return rest;
      });
    }
  }

  function replaceSpec(next: AgentSpec) {
    // Existing agents keep their identity outside Raw JSON; new agents can
    // take `id` from a pasted AgentSpec.
    setSpec((current) => ({
      ...next,
      id: isNew ? next.id : current.id,
      created_at: current.created_at,
      updated_at: current.updated_at,
    }));
    setErrors({});
  }

  async function cloneFromExisting(sourceId: string) {
    if (!sourceId) return;
    try {
      const source = await configApi.get<AgentSpec>("agents", sourceId);
      // Clone the user-editable config but drop provenance (id, timestamps,
      // registry) — a clone is locally-defined, not the original record.
      const cloned = cloneAgentSpecForEditor(hydrateAgentSpec(source));
      setSpec(cloned);
      setErrors({});
      toast.success(`Cloned from "${sourceId}" — pick a new agent id and Save.`);
    } catch (err) {
      toast.error(safeErrorMessage(err));
    }
  }

  function togglePlugin(pluginId: string) {
    setSpec((current) => {
      const plugin = capabilities?.plugins.find((candidate) => candidate.id === pluginId);
      const enabling = !(current.plugin_ids ?? []).includes(pluginId);
      const { plugin_ids, active_hook_filter } = togglePluginState(
        current.plugin_ids,
        current.active_hook_filter,
        pluginId,
      );
      return {
        ...current,
        plugin_ids,
        active_hook_filter,
        sections:
          enabling && plugin?.config_schemas.length
            ? applyPluginSectionDefaults(current.sections, plugin.config_schemas)
            : current.sections,
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
        hookFilteredOut:
          selected &&
          (spec.active_hook_filter?.length ?? 0) > 0 &&
          !spec.active_hook_filter?.includes(plugin.id),
        warnings: pluginConfigWarnings(
          plugin.id,
          selected,
          hasStoredConfig,
          spec.active_hook_filter,
        ),
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
      <div className="mx-auto w-full max-w-6xl 2xl:max-w-none p-6 md:p-8">
        <div className="rounded-sm border border-line bg-surface p-6 text-sm text-fg-soft shadow-sm">
          Loading agent...
        </div>
      </div>
    );
  }
  if (agentError) {
    return (
      <div className="mx-auto w-full max-w-6xl 2xl:max-w-none p-6 md:p-8">
        <div className="rounded-sm border border-tone-error/30 bg-tone-error/10 p-4 text-sm text-tone-error">
          Agent unavailable: {agentError}
        </div>
      </div>
    );
  }
  if (isRemoteA2aAgent) {
    return (
      <RemoteA2aAgentReadOnlyPage
        spec={spec}
        sourceState={sourceState}
        a2aServerId={a2aServerId}
        agentMetaError={agentMetaError}
      />
    );
  }

  return (
    <div className="mx-auto w-full max-w-[96rem] 2xl:max-w-none">
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
        <div className="mx-6 mt-4 rounded-sm border border-tone-error/30 bg-tone-error/10 px-4 py-3 text-sm text-tone-error md:mx-8">
          Agent metadata unavailable: {agentMetaError}
        </div>
      )}

      {overriddenFields.has("endpoint") && (
        // The admin-console editor treats `endpoint` as a locked field
        // and intentionally does NOT expose UI for editing it. The
        // server-side config API still accepts `endpoint` patches (see
        // `AgentSpecPatch::endpoint`), so a CLI or scripted client can
        // install an override that bypasses this editor. Surface that
        // existence to operators so the editor doesn't silently lie
        // about the agent's effective shape.
        <div
          className="mx-6 mt-4 rounded-sm border border-tone-warn/40 bg-tone-warn/10 px-4 py-3 text-sm text-fg md:mx-8"
          data-testid="endpoint-override-banner"
        >
          <div className="font-semibold text-tone-warn">
            This agent has an <span className="font-mono">endpoint</span> override set through the
            config API.
          </div>
          <p className="mt-1 text-xs text-fg-soft">
            The editor does not expose <span className="font-mono">endpoint</span> editing —
            programmatic clients (CLI, scripts) installed this override. To inspect or remove it,
            use <span className="font-mono">PATCH /v1/config/agents/{spec.id}/overrides</span> with{" "}
            <span className="font-mono">{`{"_clear": ["endpoint"]}`}</span>.
          </p>
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
                <div className="space-y-6">
                  <BasicsPanel
                    spec={spec}
                    capabilities={capabilities}
                    isNew={isNew}
                    updateField={updateField}
                    updateBackend={updateBackend}
                    errors={errors}
                    canResetFields={!isNew && isCustomized}
                    overriddenFields={overriddenFields}
                    onResetField={(field) => void handleResetField(field)}
                    onCloneFrom={(sourceId) => void cloneFromExisting(sourceId)}
                  />
                  {currentBackend(spec).kind !== AWAKEN_BACKEND_KIND ? (
                    <BackendConfigSection
                      backend={currentBackend(spec)}
                      capabilities={capabilities}
                      onChange={(config) => updateBackend(currentBackend(spec).kind, config)}
                    />
                  ) : null}
                </div>
              )}
              {tab.id === "tools" && (
                <ToolsPanel
                  spec={spec}
                  capabilities={capabilities}
                  updateField={updateField}
                  agentSaved={!isNew && savedSpec !== null}
                  savedSpec={savedSpec}
                  mcpServers={mcpServersQuery.data?.items ?? null}
                  mcpLoading={
                    mcpServersQuery.isPending && mcpServersQuery.fetchStatus === "fetching"
                  }
                  mcpError={mcpServersError}
                  toolMetaItems={toolMetaQuery.data ?? []}
                  toolMetaLoading={
                    toolMetaQuery.isPending && toolMetaQuery.fetchStatus === "fetching"
                  }
                  toolMetaError={toolMetaError}
                />
              )}
              {tab.id === "skills" && (
                <SkillsPanel
                  spec={spec}
                  capabilities={capabilities}
                  updateField={updateField}
                  onNavigate={setActiveTab}
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
                  updateField={updateField}
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
                <AdvancedPanel
                  spec={spec}
                  isNew={isNew}
                  updateField={updateField}
                  updateSection={updateSection}
                  replaceSpec={replaceSpec}
                />
              )}
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

        <aside className="space-y-4">
          <AgentPreviewPanel draft={spec} traceAgentId={isNew ? undefined : savedSpec?.id} />
          <AgentFrontendIntegrationCard agentId={savedSpec?.id} />
        </aside>
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

function EditorSaveBar({
  isDirty,
  isNew,
  saving,
  saveDisabled,
  spec,
  savedSpec,
  onSave,
}: {
  isDirty: boolean;
  isNew: boolean;
  saving: boolean;
  saveDisabled: boolean;
  spec: AgentSpec;
  savedSpec: AgentSpec | null;
  onSave: () => void;
}) {
  const { t } = useTranslation();
  const toast = useToast();
  const [validating, setValidating] = useState(false);
  const [diffOpen, setDiffOpen] = useState(false);

  if (!isDirty && !isNew) return null;

  async function handleValidate() {
    setValidating(true);
    try {
      await configApi.validateConfig("agents", spec, isNew ? undefined : { id: spec.id });
      toast.success("Validation passed — payload is safe to publish.");
    } catch (err) {
      toast.error(`Validation failed: ${safeErrorMessage(err)}`);
    } finally {
      setValidating(false);
    }
  }

  return (
    <>
      <div className="sticky bottom-0 z-20 mx-6 mb-4 rounded-sm border border-line bg-surface px-4 py-3 shadow-card-lift md:mx-8">
        <div className="flex flex-wrap items-center gap-3">
          <span aria-hidden className="inline-block h-2 w-2 rounded-pill bg-state-progress" />
          <div className="text-sm text-fg">
            {isNew ? (
              <span className="text-fg-strong">{t("editor.unsavedChanges")}</span>
            ) : (
              <span className="text-fg-strong">{t("editor.unsavedChanges")}</span>
            )}
            <span className="ml-2 text-fg-soft">Save will publish to the runtime config.</span>
          </div>
          <div className="ml-auto flex items-center gap-2">
            {!isNew && savedSpec && (
              <button
                type="button"
                onClick={() => setDiffOpen(true)}
                className="inline-flex h-9 items-center rounded-sm border border-line bg-surface px-3 text-sm font-medium text-fg-soft transition-colors hover:text-fg"
              >
                {t("editor.diff")}
              </button>
            )}
            <button
              type="button"
              onClick={() => void handleValidate()}
              disabled={validating || saving}
              className="inline-flex h-9 items-center rounded-sm border border-line-strong bg-surface px-3 text-sm font-medium text-fg transition-colors hover:bg-soft disabled:cursor-not-allowed disabled:opacity-60"
            >
              {validating ? t("editor.validating") : t("editor.validate")}
            </button>
            <button
              type="button"
              onClick={onSave}
              disabled={saveDisabled || validating}
              className="inline-flex h-9 items-center rounded-sm bg-accent px-4 text-sm font-medium text-accent-text transition-colors hover:opacity-90 disabled:cursor-not-allowed disabled:opacity-60"
            >
              {saving ? t("editor.saving") : t("editor.save")}
            </button>
          </div>
        </div>
      </div>

      {diffOpen && savedSpec && (
        <DiffModal current={spec} previous={savedSpec} onClose={() => setDiffOpen(false)} />
      )}
    </>
  );
}

export function DiffModal({
  current,
  previous,
  onClose,
}: {
  current: AgentSpec;
  previous: AgentSpec;
  onClose: () => void;
}) {
  // Diff against the raw values so a secret rotation still appears as a
  // semantic change, then render only redacted before/after values.
  const changes = computeRedactedDiff(
    previous as unknown as Record<string, unknown>,
    current as unknown as Record<string, unknown>,
  );
  return (
    <div
      role="dialog"
      aria-modal="true"
      aria-label="Diff vs published"
      className="fixed inset-0 z-50 flex items-center justify-center bg-overlay px-4 backdrop-blur-sm"
      onClick={onClose}
    >
      <div
        className="w-full max-w-3xl max-h-[80vh] overflow-hidden rounded-lg bg-surface shadow-overlay flex flex-col"
        onClick={(e) => e.stopPropagation()}
      >
        <div className="flex items-center justify-between border-b border-line px-5 py-3">
          <div>
            <h3 className="text-base font-semibold text-fg-strong">Diff vs published</h3>
            <p className="mt-0.5 text-xs text-fg-soft">
              {changes.length} field{changes.length === 1 ? "" : "s"} would change on save.
            </p>
          </div>
          <button
            type="button"
            onClick={onClose}
            className="rounded-sm border border-line bg-soft px-2 py-1 text-xs text-fg-soft hover:text-fg"
          >
            Close
          </button>
        </div>
        <div className="overflow-y-auto p-5">
          {changes.length === 0 ? (
            <p className="text-sm text-fg-soft">
              No semantic changes. (The dirty flag may be set because of a transient form edit;
              saving is safe.)
            </p>
          ) : (
            <ul className="space-y-3">
              {changes.map((change) => (
                <li key={change.path} className="rounded-sm border border-line bg-soft p-3">
                  <div className="flex flex-wrap items-center gap-2">
                    <div className="font-mono text-xs font-medium text-fg-strong">
                      {change.path}
                    </div>
                    {change.redactedValueChanged ? (
                      <span
                        className="rounded-pill bg-tone-warn/15 px-2 py-0.5 text-[10px] font-medium text-tone-warn"
                        data-testid="diff-redacted-value-changed"
                      >
                        changed behind redaction
                      </span>
                    ) : null}
                  </div>
                  <div className="mt-2 grid gap-2 md:grid-cols-2">
                    <div>
                      <div className="text-[10px] font-medium uppercase tracking-eyebrow text-tone-error">
                        Was
                      </div>
                      <pre className="mt-1 overflow-auto rounded border border-tone-error/20 bg-tone-error/5 px-2 py-1 font-mono text-xs text-fg">
                        {formatDiffValue(change.before, change.redactedValueChanged)}
                      </pre>
                    </div>
                    <div>
                      <div className="text-[10px] font-medium uppercase tracking-eyebrow text-tone-success">
                        Will be
                      </div>
                      <pre className="mt-1 overflow-auto rounded border border-tone-success/20 bg-tone-success/5 px-2 py-1 font-mono text-xs text-fg">
                        {formatDiffValue(change.after, change.redactedValueChanged)}
                      </pre>
                    </div>
                  </div>
                </li>
              ))}
            </ul>
          )}
        </div>
      </div>
    </div>
  );
}

function formatDiffValue(value: unknown, redactedValueChanged = false): string {
  const suffix = redactedValueChanged ? " (changed)" : "";
  if (value === undefined) return `(unset)${suffix}`;
  if (value === null) return `null${suffix}`;
  if (typeof value === "string") return `${redactSecretString(value) || "(empty string)"}${suffix}`;
  // Defense-in-depth: the caller is expected to have already redacted, but
  // a future code path that forgets shouldn't end up dumping secrets here.
  const rendered = JSON.stringify(redactSecretsForDisplay(value), null, 2);
  return redactedValueChanged ? `${rendered}\n(changed)` : rendered;
}

function StickyEditorHeader({
  isNew,
  agentId,
  spec,
  isDirty,
  saveDisabled,
  onSave,
  activeTab,
  onTabChange,
  sourceState,
  onResetOverrides,
}: {
  isNew: boolean;
  agentId: string;
  spec: AgentSpec;
  isDirty: boolean;
  saveDisabled: boolean;
  onSave: () => void;
  activeTab: AgentEditorTabId;
  onTabChange: (next: AgentEditorTabId) => void;
  sourceState: ConfigSourceState | null;
  onResetOverrides: () => void;
}) {
  const { t } = useTranslation();
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
          <div className="flex items-center gap-2">
            <Link
              to={adminRoutes.agents}
              aria-label="Back to agents"
              title="Back to agents"
              className="inline-flex h-7 w-7 items-center justify-center rounded-sm text-fg-soft transition hover:bg-soft hover:text-fg"
            >
              <svg
                aria-hidden
                viewBox="0 0 24 24"
                className="h-4 w-4"
                fill="none"
                stroke="currentColor"
                strokeWidth="2"
                strokeLinecap="round"
                strokeLinejoin="round"
              >
                <path d="M15 18l-6-6 6-6" />
              </svg>
            </Link>
            {!isNew && agentId && (
              <Link
                to={adminRoutes.auditLogForResource(`agents/${agentId}`)}
                className="rounded-sm border border-line-strong bg-surface px-2.5 py-1 text-xs font-medium text-fg-soft transition hover:bg-soft hover:text-fg"
              >
                {t("editor.history")}
              </Link>
            )}
          </div>
          <h2 className="mt-2 flex flex-wrap items-center gap-3 text-3xl font-semibold text-fg-strong">
            <span>{isNew ? t("editor.newTitle") : t("editor.editTitle", { id: agentId })}</span>
            {isDirty ? (
              <span className="rounded-full bg-tone-warn/15 px-2 py-0.5 text-xs font-medium uppercase tracking-wide text-tone-warn">
                {t("editor.unsavedChanges")}
              </span>
            ) : !isNew ? (
              <span className="rounded-full bg-tone-success/15 px-2 py-0.5 text-xs font-medium uppercase tracking-wide text-tone-success">
                {t("editor.upToDate")}
              </span>
            ) : null}
            {!isNew && sourceState && <EditorSourceBadge state={sourceState} />}
          </h2>
          {!isNew && sourceState === "customized" && (
            <div className="mt-1">
              <button
                type="button"
                onClick={onResetOverrides}
                className="text-xs font-medium text-tone-error transition hover:underline"
              >
                {t("agents.resetOverrides")}
              </button>
            </div>
          )}
        </div>
        {isDirty || isNew ? null : (
          <button
            type="button"
            onClick={onSave}
            disabled={saveDisabled}
            className="rounded-sm bg-accent px-4 py-2 text-sm font-medium text-accent-text transition hover:opacity-90 disabled:cursor-not-allowed disabled:opacity-60"
          >
            {t("editor.save")}
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

function ToolsPanel({
  spec,
  capabilities,
  updateField,
  agentSaved,
  savedSpec,
  mcpServers,
  mcpLoading,
  mcpError,
  toolMetaItems,
  toolMetaLoading,
  toolMetaError,
}: {
  spec: AgentSpec;
  capabilities: Capabilities | null;
  updateField: <K extends keyof AgentSpec>(key: K, value: AgentSpec[K]) => void;
  agentSaved: boolean;
  savedSpec: AgentSpec | null;
  mcpServers: McpServerRecord[] | null;
  mcpLoading: boolean;
  mcpError: string | null;
  toolMetaItems: ConfigMetaItem[];
  toolMetaLoading: boolean;
  toolMetaError: string | null;
}) {
  // Loading-only gate (empty registry must still render pattern editors).
  if (!capabilities) {
    return (
      <div className="rounded-sm border border-dashed border-line bg-surface p-6 text-sm text-fg-soft">
        Loading published tool capabilities...
      </div>
    );
  }
  return (
    <div className="space-y-6">
      <ToolSelectorsPanel spec={spec} capabilities={capabilities} updateField={updateField} />
      <AdminAssistantLockedToolsSection capabilities={capabilities} />
      <McpServersPanel
        spec={spec}
        servers={mcpServers}
        loading={mcpLoading}
        error={mcpError}
        updateField={updateField}
      />
      <AllowedExcludedToolsSection
        spec={spec}
        capabilities={capabilities}
        agentSaved={agentSaved}
        savedSpec={savedSpec}
        toolMetaItems={toolMetaItems}
        toolMetaLoading={toolMetaLoading}
        toolMetaError={toolMetaError}
      />
    </div>
  );
}

/** Pre-permission visible tool set: mirrors Rust's `AgentSpec::tool_allowed`
 * via the shared TS matcher. Permission BeforeInference hook may further
 * filter at runtime. */
function computeAllowedTools(tools: Capabilities["tools"], s: AgentSpec): Capabilities["tools"] {
  const c: AgentSpecCatalog = {
    allowed_tools: s.allowed_tools ?? undefined,
    allowed_tool_patterns: s.allowed_tool_patterns ?? undefined,
    excluded_tools: s.excluded_tools ?? undefined,
    excluded_tool_patterns: s.excluded_tool_patterns ?? undefined,
  };
  return tools.filter((t) => isToolAllowed(c, t.id));
}

function AllowedExcludedToolsSection({
  spec,
  capabilities,
  agentSaved,
  savedSpec,
  toolMetaItems,
  toolMetaLoading,
  toolMetaError,
}: {
  spec: AgentSpec;
  capabilities: Capabilities;
  /** `true` once the agent exists server-side; the preview endpoint reads
   *  the stored record so it would 404 for an in-flight new draft. */
  agentSaved: boolean;
  /** The last-saved spec from `useConfigRecordQuery`. The preview gate
   *  reads this rather than the working draft so the UI never claims a
   *  preview is available based on an unsaved plugin toggle. */
  savedSpec: AgentSpec | null;
  toolMetaItems: ConfigMetaItem[];
  toolMetaLoading: boolean;
  toolMetaError: string | null;
}) {
  const visible = useMemo(
    () => computeAllowedTools(capabilities.tools, spec),
    [capabilities.tools, spec],
  );
  const toolMetaById = useMemo(() => {
    const map = new Map<string, RecordMeta>();
    for (const item of toolMetaItems) {
      map.set(item.id, item.meta);
    }
    return map;
  }, [toolMetaItems]);
  const total = capabilities.tools.length;
  const excludedCount =
    (spec.excluded_tools ?? []).length + (spec.excluded_tool_patterns ?? []).length;
  const allowedMode = deriveAllowedMode(spec);
  const allowedSize = visible.length;
  // Gate on the SAVED spec — not the draft. The preview endpoint reads the
  // persisted record, so showing/hiding the block based on a dirty draft
  // would silently lie to the user: "you toggled permission on in the
  // draft, here's the preview" (but it's actually computed against the
  // saved version that doesn't have the plugin). Conversely, disabling
  // the plugin in the draft would hide the preview even though the saved
  // agent is still permission-gated at runtime.
  //
  // "Enabled" also requires `active_hook_filter` to admit permission
  // hooks. Mirrors the server's runtime: an empty filter runs all hooks;
  // a non-empty filter only runs the listed plugins' hooks. So a saved
  // agent with `plugin_ids: ["permission"]` but
  // `active_hook_filter: ["observability"]` would not run permission
  // hooks at runtime, and the preview must not claim to show the
  // post-filter tool set.
  const savedPluginIds = savedSpec?.plugin_ids ?? [];
  const savedHookFilter = savedSpec?.active_hook_filter ?? [];
  const permissionLoaded = savedPluginIds.includes("permission");
  const permissionHooksActive =
    savedHookFilter.length === 0 || savedHookFilter.includes("permission");
  const permissionPluginEnabled = permissionLoaded && permissionHooksActive;
  // Surface a stale-preview hint when ANY of the fields the preview is
  // computed from differs between draft and saved. Plugin id toggles,
  // hook-filter changes, allowed/excluded tool list edits, and edits to
  // the `permission` section all change what the server-side preview
  // would return — but the preview query reads only the saved record,
  // so without this hint the top section (computed from draft) and the
  // preview block (computed from saved) silently disagree.
  const draftPluginIds = spec.plugin_ids ?? [];
  const draftHookFilter = spec.active_hook_filter ?? [];
  const draftPermissionSection = (spec.sections ?? {})["permission"];
  const savedPermissionSection = (savedSpec?.sections ?? {})["permission"];
  const catalogFieldsDirty = (
    ["allowed_tools", "allowed_tool_patterns", "excluded_tools", "excluded_tool_patterns"] as const
  ).some((f) => !deepEqualCanonical(spec[f] ?? null, savedSpec?.[f] ?? null));
  const previewInputsDirty =
    agentSaved &&
    (canonicalStringify([...draftPluginIds].sort()) !==
      canonicalStringify([...savedPluginIds].sort()) ||
      canonicalStringify([...draftHookFilter].sort()) !==
        canonicalStringify([...savedHookFilter].sort()) ||
      catalogFieldsDirty ||
      !deepEqualCanonical(draftPermissionSection, savedPermissionSection));
  // Fetch the server-computed permission preview when the agent is saved
  // AND the saved spec has the permission plugin enabled.
  const previewQuery = useQuery({
    queryKey: qk.agent.permissionPreview(spec.id),
    queryFn: () => configApi.agentPermissionPreview(spec.id),
    enabled: agentSaved && permissionPluginEnabled && spec.id.trim().length > 0,
    staleTime: 30_000,
  });
  return (
    <section className="rounded-sm border border-line bg-surface p-5 shadow-sm">
      <div className="flex flex-wrap items-start justify-between gap-3">
        <div>
          <h3 className="text-lg font-semibold text-fg-strong">
            Allowed/excluded tools (pre-permission)
          </h3>
          <p className="mt-2 max-w-xl text-sm text-fg-soft">
            <span className="font-mono">allowed_tools</span> ∖{" "}
            <span className="font-mono">excluded_tools</span> over the published tool set —{" "}
            <em>after</em> allow/exclude lists, <em>before</em> runtime plugin filtering. The
            permission plugin (and any other plugin running a BeforeInference hook) may still gate
            or rewrite this list per call, so this is a candidate set, not a strict superset of what
            the model finally sees.{" "}
            {permissionPluginEnabled
              ? "The permission preview below shows the server-computed effective list."
              : null}
          </p>
        </div>
        <div className="text-right">
          <div className="font-mono text-xl font-semibold text-fg-strong">
            {visible.length}
            <span className="text-fg-faint"> / {total}</span>
          </div>
          <div className="text-[10px] font-medium uppercase tracking-eyebrow text-fg-soft">
            tools after lists
          </div>
        </div>
      </div>

      <div className="mt-3 flex flex-wrap gap-2 text-[11px]">
        <span className="rounded-pill bg-muted px-2 py-0.5 text-fg-soft">
          Allowed: {allowedMode === "all" ? "all" : `${allowedSize}`}
        </span>
        <span className="rounded-pill bg-muted px-2 py-0.5 text-fg-soft">
          Excluded: {excludedCount}
        </span>
      </div>

      {visible.length === 0 ? (
        <div className="mt-4 rounded-sm border border-tone-warn/35 bg-tone-warn/10 px-4 py-3 text-sm text-tone-warn">
          The allow/exclude lists leave no tools. Combined with any permission policy this means the
          model will see an empty tool set.
        </div>
      ) : (
        <VisibleToolDescriptors
          tools={visible}
          toolMetaById={toolMetaById}
          metadataLoading={toolMetaLoading}
          metadataError={toolMetaError}
        />
      )}

      {permissionPluginEnabled ? (
        <>
          {previewInputsDirty ? (
            <div
              className="mt-4 rounded-sm border border-tone-warn/35 bg-tone-warn/10 px-3 py-2 text-xs text-tone-warn"
              data-testid="permission-preview-dirty-hint"
            >
              The tools-after-lists count above reflects your unsaved draft; the permission preview
              below reflects the <em>saved</em> config. Save to align them — preview inputs
              (plugins, hook filter, allow/exclude lists, permission rules) have unsaved changes.
            </div>
          ) : null}
          <PermissionPreviewBlock
            agentSaved={agentSaved}
            loading={previewQuery.isPending && previewQuery.fetchStatus === "fetching"}
            error={previewQuery.error}
            preview={previewQuery.data}
          />
        </>
      ) : null}
    </section>
  );
}

function PermissionPreviewBlock({
  agentSaved,
  loading,
  error,
  preview,
}: {
  agentSaved: boolean;
  loading: boolean;
  error: unknown;
  preview: PermissionPreviewResponse | null | undefined;
}) {
  if (!agentSaved) {
    return (
      <div
        className="mt-4 rounded-sm border border-dashed border-line bg-soft px-3 py-2 text-xs text-fg-soft"
        data-testid="permission-preview-pending-save"
      >
        Permission preview will be available after the agent is saved — the server reads the
        persisted record to compute true effective tools.
      </div>
    );
  }
  if (loading) {
    return (
      <div className="mt-4 rounded-sm border border-line bg-soft px-3 py-2 text-xs text-fg-soft">
        Loading permission preview…
      </div>
    );
  }
  if (error) {
    return (
      <div className="mt-4 rounded-sm border border-tone-error/30 bg-tone-error/10 px-3 py-2 text-xs text-tone-error">
        Failed to load permission preview: {safeErrorMessage(error)}
      </div>
    );
  }
  if (preview === null) {
    return (
      <div className="mt-4 rounded-sm border border-dashed border-line bg-soft px-3 py-2 text-xs text-fg-soft">
        Server build lacks the <span className="font-mono">permission</span> feature — preview
        unavailable.
      </div>
    );
  }
  if (!preview) return null;

  return (
    <div
      data-testid="permission-preview-block"
      className="mt-4 space-y-3 rounded-sm border border-line bg-soft px-3 py-3"
    >
      <div className="flex flex-wrap items-baseline justify-between gap-2 text-xs">
        <div>
          <div className="text-[10px] font-medium uppercase tracking-eyebrow text-fg-soft">
            Permission preview (server-computed)
          </div>
          <div className="mt-1 text-fg-soft">
            Default behavior: <span className="font-mono">{preview.default_behavior ?? "—"}</span>
            <span className="mx-2 text-fg-faint">·</span>
            Effective:{" "}
            <span className="font-mono text-fg-strong">{preview.effective_tools.length}</span>
            <span className="mx-2 text-fg-faint">·</span>
            Unconditionally denied:{" "}
            <span className="font-mono text-fg-strong">
              {preview.unconditionally_denied.length}
            </span>
            <span className="mx-2 text-fg-faint">·</span>
            Args-conditional rules:{" "}
            <span className="font-mono text-fg-strong">
              {preview.args_conditional_rules.length}
            </span>
          </div>
        </div>
      </div>

      {preview.unconditionally_denied.length > 0 ? (
        <details className="rounded-sm border border-tone-error/30 bg-tone-error/5">
          <summary className="cursor-pointer px-3 py-2 text-xs font-medium text-tone-error">
            {preview.unconditionally_denied.length} tool
            {preview.unconditionally_denied.length === 1 ? "" : "s"} stripped before the model sees
            the list
          </summary>
          <ul className="grid gap-1 px-3 py-2 md:grid-cols-2 xl:grid-cols-3">
            {preview.unconditionally_denied.map((id) => (
              <li
                key={id}
                className="truncate font-mono text-[11px] text-tone-error"
                data-testid="permission-preview-denied-tool"
              >
                {id}
              </li>
            ))}
          </ul>
        </details>
      ) : null}

      <details className="rounded-sm border border-line bg-surface">
        <summary className="cursor-pointer px-3 py-2 text-xs font-medium text-fg-soft">
          Show {preview.effective_tools.length} effective tool
          {preview.effective_tools.length === 1 ? "" : "s"}
        </summary>
        <ul className="grid gap-1 px-3 py-2 md:grid-cols-2 xl:grid-cols-3">
          {preview.effective_tools.map((id) => (
            <li
              key={id}
              className="truncate font-mono text-[11px] text-fg-strong"
              data-testid="permission-preview-effective-tool"
            >
              {id}
            </li>
          ))}
        </ul>
      </details>

      {preview.args_conditional_rules.length > 0 ? (
        <details className="rounded-sm border border-tone-warn/40 bg-tone-warn/5">
          <summary className="cursor-pointer px-3 py-2 text-xs font-medium text-tone-warn">
            {preview.args_conditional_rules.length} rule
            {preview.args_conditional_rules.length === 1 ? "" : "s"} depend on call arguments
          </summary>
          <ul className="grid gap-1 px-3 py-2">
            {preview.args_conditional_rules.map((rule, idx) => (
              <li
                key={`${rule.tool}:${idx}`}
                className="font-mono text-[11px] text-fg"
                data-testid="permission-preview-conditional-rule"
              >
                <span className="font-semibold">{rule.behavior}</span>{" "}
                <span className="text-fg-soft">{rule.pattern}</span>
              </li>
            ))}
          </ul>
        </details>
      ) : null}
    </div>
  );
}

function AdvancedPanel({
  spec,
  isNew,
  updateField,
  updateSection,
  replaceSpec,
}: {
  spec: AgentSpec;
  isNew: boolean;
  updateField: <K extends keyof AgentSpec>(key: K, value: AgentSpec[K]) => void;
  updateSection: (key: string, value: unknown) => void;
  replaceSpec: (next: AgentSpec) => void;
}) {
  return (
    <div className="space-y-6">
      <ContextPolicySection
        value={spec.context_policy ?? null}
        onChange={(next) => updateField("context_policy", next)}
      />
      <CompactionSection
        value={spec.sections?.compaction}
        onChange={(next) => updateSection("compaction", next ?? undefined)}
      />
      {spec.endpoint ? <RemoteEndpointReadonlySection endpoint={spec.endpoint} /> : null}
      <JsonEditorSection spec={spec} isNew={isNew} replaceSpec={replaceSpec} />
    </div>
  );
}

function JsonEditorSection({
  spec,
  isNew,
  replaceSpec,
}: {
  spec: AgentSpec;
  isNew: boolean;
  replaceSpec: (next: AgentSpec) => void;
}) {
  // Raw JSON uses a redacted display copy. Apply validates against that copy
  // before restoring unchanged markers and overlaying locked real values.
  const editingSpec = useMemo(() => redactAgentSpecForEditing(spec), [spec]);
  const specSerialized = useMemo(
    () => JSON.stringify(editingSpec.redacted, null, 2),
    [editingSpec],
  );
  const [draft, setDraft] = useState<string>(() => specSerialized);
  const [error, setError] = useState<string | null>(null);
  const [touched, setTouched] = useState(false);
  const [applying, setApplying] = useState(false);
  const toast = useToast();
  const hasRedactedSecrets = editingSpec.redactions.length > 0;

  // Re-sync the textarea when the underlying spec changes from elsewhere
  // (e.g. another tab edits, restore from history) and the user has not yet
  // made unsaved edits in this textarea.
  useEffect(() => {
    if (!touched) {
      setDraft(specSerialized);
    }
  }, [specSerialized, touched]);

  async function handleApply() {
    let parsed: unknown;
    try {
      parsed = JSON.parse(draft);
    } catch (err) {
      setError(safeErrorMessage(err));
      return;
    }
    if (!parsed || typeof parsed !== "object" || Array.isArray(parsed)) {
      setError("Top-level value must be a JSON object.");
      return;
    }
    const parsedRecord = parsed as Record<string, unknown>;
    // Save persists only known identity, locked, and patchable fields.
    const unknown = unknownAgentSpecFields(parsedRecord);
    if (unknown.length > 0) {
      // Treat unknown fields as console/schema drift, not malformed JSON.
      setError(
        `This admin console version does not recognize ${
          unknown.length === 1 ? "field" : "fields"
        }: ${unknown.map((k) => `\`${k}\``).join(", ")}. Either upgrade the console, ` +
          `or remove the field from this draft.`,
      );
      return;
    }
    // Existing identity and timestamp edits would be overwritten below, so
    // reject them before Apply can look successful.
    const identityFields: Array<keyof AgentSpec> = isNew
      ? ["created_at", "updated_at"]
      : ["id", "created_at", "updated_at"];
    for (const field of identityFields) {
      if (!(field in parsedRecord)) continue;
      if (!deepEqualCanonical(parsedRecord[field], editingSpec.redacted[field])) {
        setError(
          `\`${field}\` is a server-managed identity / timestamp field and can't be edited from Raw JSON. Revert it to its current value to apply.`,
        );
        return;
      }
    }
    // Compare before mergeLockedFields; overlaying first would hide edits to
    // endpoint / registry and silently drop them.
    const displaySpec = editingSpec.redacted;
    const lockedField = lockedFieldChange(displaySpec, parsedRecord);
    if (lockedField) {
      setError(
        `\`${lockedField}\` can't be changed from the editor — it's a provenance / runtime-locality field. Revert the key to its current value to apply.`,
      );
      return;
    }
    const changedMarkerPaths = changedRedactionMarkerPaths(parsedRecord, editingSpec.redactions);
    if (changedMarkerPaths.length > 0) {
      setError(
        `Redaction marker \`${changedMarkerPaths[0]}\` is inside an edited value. Replace the full credential value or revert the marker before applying.`,
      );
      return;
    }
    const withRestoredRedactions = restoreUnchangedRedactions(
      parsedRecord,
      editingSpec.redactions,
    ) as Record<string, unknown>;
    // Restore the real locked values after all Raw JSON edit checks.
    const withRealLockedFields = mergeLockedFields(withRestoredRedactions, spec);
    // Server validation runs before the draft state mutates.
    const candidateSource = withRealLockedFields as unknown as AgentSpec;
    const candidate: AgentSpec = {
      ...candidateSource,
      id: isNew ? candidateSource.id : spec.id,
      created_at: spec.created_at,
      updated_at: spec.updated_at,
    };
    setApplying(true);
    try {
      await configApi.validateConfig("agents", candidate, isNew ? undefined : { id: spec.id });
      replaceSpec(candidate);
      setError(null);
      setTouched(false);
      toast.success("JSON applied to draft. Click Save to publish.");
    } catch (err) {
      setError(`Validation failed: ${safeErrorMessage(err)}`);
    } finally {
      setApplying(false);
    }
  }

  function handleReset() {
    setDraft(specSerialized);
    setError(null);
    setTouched(false);
  }

  return (
    <section className="rounded-sm border border-line bg-surface p-5 shadow-sm">
      <div className="flex flex-wrap items-start justify-between gap-3">
        <div>
          <h3 className="text-lg font-semibold text-fg-strong">Raw JSON</h3>
          <p className="mt-2 max-w-xl text-sm text-fg-soft">
            Edit the AgentSpec payload directly.{" "}
            {isNew ? (
              <>
                <span className="font-mono">id</span> can be set for new agents;{" "}
                <span className="font-mono">created_at</span> and{" "}
                <span className="font-mono">updated_at</span> are preserved on Apply.
              </>
            ) : (
              <>
                <span className="font-mono">id</span>, <span className="font-mono">created_at</span>
                , and <span className="font-mono">updated_at</span> are preserved on Apply.
              </>
            )}{" "}
            Click Save below to publish — the runtime validation still runs.
          </p>
          <p
            className="mt-2 max-w-xl text-xs text-fg-soft"
            data-testid="raw-json-locked-field-help"
          >
            Locked fields are normalized on Apply. For <span className="font-mono">endpoint</span>{" "}
            and <span className="font-mono">registry</span> specifically, an explicit{" "}
            <span className="font-mono">null</span> is treated the same as absence when the current
            spec has no value — both mean "no override here". Use the Customization controls above
            to clear an existing override.
          </p>
          {hasRedactedSecrets ? (
            <p
              className="mt-2 max-w-xl text-xs text-fg-soft"
              data-testid="raw-json-redaction-notice"
            >
              Credential-like fields are masked as
              <span className="mx-1 font-mono">__AWAKEN_REDACTED_SECRET_...__</span>
              in this view. Apply restores unchanged redaction markers automatically;{" "}
              <span className="font-mono">endpoint</span> and{" "}
              <span className="font-mono">registry</span> remain read-only from this editor.
            </p>
          ) : null}
        </div>
        <div className="flex flex-wrap gap-2">
          <button
            type="button"
            onClick={handleReset}
            disabled={!touched || applying}
            className="rounded-sm border border-line-strong bg-surface px-3 py-1.5 text-xs font-medium text-fg-soft transition hover:bg-soft disabled:cursor-not-allowed disabled:opacity-60"
          >
            Reset
          </button>
          <button
            type="button"
            onClick={() => void handleApply()}
            disabled={!touched || applying}
            className="rounded-sm bg-accent px-3 py-1.5 text-xs font-medium text-accent-text transition hover:opacity-90 disabled:cursor-not-allowed disabled:opacity-60"
          >
            {applying ? "Validating…" : "Apply to draft"}
          </button>
        </div>
      </div>

      <textarea
        value={draft}
        onChange={(event) => {
          setDraft(event.target.value);
          setTouched(true);
          if (error) setError(null);
        }}
        spellCheck={false}
        rows={20}
        className="mt-4 w-full rounded-sm border border-line-strong bg-code-bg px-3 py-2 font-mono text-xs leading-5 text-code-fg outline-none transition focus:border-fg"
      />

      {error ? (
        <div className="mt-3 rounded-sm border border-tone-error/30 bg-tone-error/10 px-3 py-2 text-xs text-tone-error">
          {error}
        </div>
      ) : null}

      {touched ? (
        <div className="mt-3 text-xs text-fg-soft">
          Unsaved JSON edits — click <strong>Apply to draft</strong> to fold them into the form,
          then Save.
        </div>
      ) : null}
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
  const [selectedEvent, setSelectedEvent] = useState<AuditEvent | null>(null);
  const [restoring, setRestoring] = useState<string | null>(null);
  const historyQuery = useAuditLogInfiniteQuery(
    { resource: `agents/${spec.id}`, limit: 50 },
    { enabled: !isNew && Boolean(spec.id) },
  );
  const page = historyQuery.data?.pages[0] ?? null;
  const loading = historyQuery.isFetching;
  const error = historyQuery.error ? safeErrorMessage(historyQuery.error) : null;
  const refetchHistory = historyQuery.refetch;

  useEffect(() => {
    if (refreshKey > 0) {
      void refetchHistory();
    }
  }, [refetchHistory, refreshKey]);

  async function handleRestore(event: AuditEvent) {
    const targetSpec = event.action === "delete" ? event.before : event.after;
    // R12 #2 — Two-layer redaction before the confirm-dialog DOM:
    //   1. `redactAgentSpecForDisplay` applies default-deny on
    //      `endpoint.auth` (every key except `type`).
    //   2. `redactSecretsForDisplay` then walks the whole tree and
    //      masks pattern-matched secret keys anywhere — `sections.*`
    //      is a free-form `Record<string, unknown>` and can carry
    //      plugin / provider credentials (`api_key`, `bearer_token`,
    //      `cookie`, `jwt`, etc.). Without the second pass, a restore
    //      preview of an agent whose `sections.observability.api_key`
    //      contained a live key would render that key into the DOM.
    // The real values survive in the editor's spec state and are still
    // what gets POSTed back to the restore endpoint; redaction is
    // purely a display concern.
    const redactedCurrent = redactSecretsForDisplay(redactAgentSpecForDisplay(spec));
    const redactedTarget =
      targetSpec && typeof targetSpec === "object"
        ? redactSecretsForDisplay(redactAgentSpecForDisplay(targetSpec as unknown as AgentSpec))
        : null;
    const confirmed = await confirm({
      title: "Restore agent to this version?",
      description: (
        <div className="space-y-3">
          <p className="text-xs text-fg-soft">
            Restoring will overwrite the current agent configuration with the version from this
            event.
          </p>
          <div className="grid grid-cols-2 gap-3">
            <div>
              <p className="mb-1 text-xs font-medium uppercase tracking-wide text-fg-soft">
                Current
              </p>
              <pre className="max-h-48 overflow-auto rounded-sm border border-line bg-soft p-2 text-xs text-fg">
                {JSON.stringify(redactedCurrent, null, 2)}
              </pre>
            </div>
            <div>
              <p className="mb-1 text-xs font-medium uppercase tracking-wide text-fg-soft">
                This version
              </p>
              <pre className="max-h-48 overflow-auto rounded-sm border border-line bg-soft p-2 text-xs text-fg">
                {redactedTarget != null ? JSON.stringify(redactedTarget, null, 2) : "—"}
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
      const hydrated = hydrateAgentSpec(refreshed);
      onSpecRestored(hydrated);
      void refetchHistory();
    } catch (err) {
      toast.error(safeErrorMessage(err));
    } finally {
      setRestoring(null);
    }
  }

  if (isNew || !spec.id) {
    return (
      <section className="rounded-sm border border-dashed border-line bg-surface p-6 text-center text-sm text-fg-soft shadow-sm">
        Save the agent first to see its history.
      </section>
    );
  }

  return (
    <section className="rounded-sm border border-line bg-surface shadow-sm">
      <div className="flex items-center justify-between border-b border-line px-5 py-4">
        <h3 className="text-lg font-semibold text-fg-strong">History</h3>
        <button
          type="button"
          onClick={() => void refetchHistory()}
          disabled={loading}
          className="text-xs font-medium text-fg-soft transition hover:text-fg-strong disabled:opacity-60"
        >
          {loading ? "Loading…" : "Refresh"}
        </button>
      </div>

      {error && <div className="px-5 py-3 text-sm text-tone-error">{error}</div>}

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
                    <td className="px-4 py-3 font-mono text-xs text-fg">{ts.toLocaleString()}</td>
                    <td className="px-4 py-3 text-sm text-fg">
                      <span className="font-mono text-xs">{actor.hash}</span>
                      {actor.label && <span className="ml-1 text-fg-soft">/{actor.label}</span>}
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

function HistoryEventPanel({ event, onClose }: { event: AuditEvent; onClose: () => void }) {
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
            className="rounded-sm px-2 py-1 text-sm text-fg-soft hover:bg-muted"
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
            <pre className="overflow-auto rounded-sm border border-line bg-soft p-3 text-xs leading-relaxed text-fg">
              {event.before != null
                ? JSON.stringify(redactSecretsForDisplay(event.before), null, 2)
                : "—"}
            </pre>
          </div>
          <div>
            <p className="mb-2 text-xs font-medium uppercase tracking-wide text-fg-soft">After</p>
            <pre className="overflow-auto rounded-sm border border-line bg-soft p-3 text-xs leading-relaxed text-fg">
              {event.after != null
                ? JSON.stringify(redactSecretsForDisplay(event.after), null, 2)
                : "—"}
            </pre>
          </div>
        </div>
      </div>
    </div>
  );
}
