import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useEffect, useMemo, useRef, useState } from "react";
import { useNavigate, useParams, useSearchParams } from "react-router";
import AgentEditorHeader from "../components/agent/AgentEditorHeader";
import {
  agentModelIsRunnable, agentNeedsToolBridge,
  AgentEditorStageNavigation, AgentValidationIssues,
  AttachedAgentContext,
  BLANK_AGENT_CONFIG as BLANK,
  buildAgentDraftBody,
} from "../components/agent/AgentEditorChrome";
import AgentEditorStages from "../components/agent/AgentEditorStages";
import AgentPublicationModals, {
  type QuickRunIntent,
} from "../components/agent/AgentPublicationModals";
import {
  advancedSectionForPath,
  builderSectionForPath,
  stageForPath,
  type AdvancedSection,
  type AuthorStage,
  type BuilderSection,
} from "../components/agent/agent-editor-navigation";
import { useAgentDraftReview } from "../components/agent/useAgentDraftReview";
import ReadinessPanel from "../components/app/ReadinessPanel";
import SandboxPane from "../components/session/SandboxPane";
import Drawer from "../components/ui/Drawer";
import { useToast } from "../components/ui/Toast";
import {
  BUILTIN_LOCAL_ENVIRONMENT_ID,
  api,
  createManagedSession,
  IdempotencyScope,
  isAbsent,
  workspaceQuery,
  ws,
} from "../lib/api/client";
import type {
  AgentConfig,
  AgentConfigItem,
  AgentInputConfig,
  CreateSessionRequest,
  CredentialSource,
  InputBinding,
  PublishResult,
  ValidationIssue,
  ValidationResult,
} from "../lib/api/types";
import { reconcileMemoryBinding, shouldEnableMemoryExtraction } from "../lib/agent-memory-binding";
import {
  AUXILIARY_PARENT_KEY,
  AUXILIARY_ROLE_KEY,
  agentTarget,
  delegateTargetView,
  withRoster,
} from "../lib/agent-collaboration";
import { useApp } from "../lib/app-state";
import { useCapabilities } from "../lib/useCapabilities";
import { useConfigCapabilities } from "../lib/useConfigCapabilities";
import { hasSurface, quickstartSessionPath } from "../lib/navigation/paths";
import { useModels } from "../lib/useModels";
import { useUnsavedGuard } from "../lib/useUnsavedGuard";
import ModelsSurface from "./models";

export default function AgentEditorSurface() {
  const app = useApp();
  const nav = useNavigate();
  const qc = useQueryClient();
  const toast = useToast();
  const quickRunCreateIdentity = useRef(new IdempotencyScope("quick-run-session-create"));
  const deploymentCapabilities = useConfigCapabilities();
  const managedRuntime = hasSurface(deploymentCapabilities.data, "managed_runtime");
  const { ws: wsId = "default", id = "new" } = useParams();
  const [searchParams] = useSearchParams();
  const parentAgentId = searchParams.get("parent")?.trim() ?? "";
  const attachAuxiliaryId = searchParams.get("attach")?.trim() ?? "";
  const isNew = id === "new";
  const [stage, setStage] = useState<AuthorStage>(() => {
    const requested = searchParams.get("stage");
    return requested === "build" || requested === "advanced" ? requested : "quickstart";
  });
  const [builderSection, setBuilderSection] = useState<BuilderSection>(() => {
    const requested = searchParams.get("section");
    return ["instructions", "tools", "integrations", "knowledge"].includes(requested ?? "")
      ? requested as BuilderSection
      : "instructions";
  });
  const [advancedSection, setAdvancedSection] = useState<AdvancedSection>(() => {
    const requested = searchParams.get("section");
    return ["orchestration", "extensions", "source", "release"].includes(requested ?? "")
      ? requested as AdvancedSection
      : "orchestration";
  });
  const [showSandbox, setShowSandbox] = useState(false);
  const [showPublish, setShowPublish] = useState(false);
  const [quickRunIntent, setQuickRunIntent] = useState<QuickRunIntent>();
  const [rawValid, setRawValid] = useState(true);
  const [integrationsValid, setIntegrationsValid] = useState(true);
  const [cfg, setCfg] = useState<AgentConfig>(BLANK);
  const [permissionPreset, setPermissionPreset] = useState<"controlled_modifications" | null>(null);
  const [dirty, setDirty] = useState(false);
  const [resourceInputs, setResourceInputs] = useState<InputBinding[]>([]);
  const [resourceRevision, setResourceRevision] = useState(0);
  const [resourcesDirty, setResourcesDirty] = useState(false);
  const [issues, setIssues] = useState<ValidationIssue[]>([]);
  const [manageModels, setManageModels] = useState(false);
  const [savedId, setSavedId] = useState<string | null>(null);
  const [quickRunSessionId, setQuickRunSessionId] = useState<string | null>(null);
  const importedMemory = useRef("");
  const attachedAuxiliary = useRef("");
  const previousRouteId = useRef(id);
  const hasUnsavedChanges = dirty || resourcesDirty;
  useUnsavedGuard(
    hasUnsavedChanges,
    app.t("You have unsaved changes. Leave anyway?", "有未保存的更改，仍要离开吗？"),
  );

  const existing = useQuery({
    queryKey: ["config-agent", wsId, id],
    enabled: !isNew,
    queryFn: () => api.get<AgentConfigItem>(ws(`/v1/config/agents/${id}`)),
    retry: (attempt, error) => !isAbsent(error) && attempt < 2,
  });
  const existingResources = useQuery({
    queryKey: ["agent-resources", id],
    enabled: !isNew,
    queryFn: () => api.get<AgentInputConfig>(ws(`/v1/config/agents/${id}/resources`)),
    retry: (attempt, error) => !isAbsent(error) && attempt < 2,
  });
  const caps = useCapabilities();
  const credentials = useQuery({
    queryKey: ["credentials", wsId],
    queryFn: () => api.get<CredentialSource[]>(
      ws(workspaceQuery("/v1/config/credentials", wsId)),
    ),
  });
  const { ready: models, all: allModels } = useModels();
  const targetId = () => (isNew ? cfg.id.trim() : id);
  const canSave = rawValid
    && integrationsValid
    && targetId().length > 0
    && (cfg.system ?? "").trim().length > 0;
  const body = () => buildAgentDraftBody(cfg, targetId(), permissionPreset);
  const modelIsRunnable = agentModelIsRunnable(
    cfg.model,
    models,
    caps.data?.runtimes ?? [],
    agentNeedsToolBridge(cfg),
  );

  const saveConfig = async (): Promise<number | undefined> => {
    const result = await api.put<{ id: string; generation?: number }>(
      ws(`/v1/config/agents/${targetId()}`),
      body(),
    );
    if (result.generation !== undefined) {
      setCfg((current) => ({ ...current, generation: result.generation }));
    }
    setPermissionPreset(null);
    return result.generation;
  };
  const saveResources = async (): Promise<number> => {
    if (!resourcesDirty && !(isNew && resourceInputs.length > 0)) return resourceRevision;
    const agentId = targetId();
    if (!agentId) {
      throw new Error(app.t(
        "Enter an Agent id before saving or publishing.",
        "保存或发布前请输入 Agent id。",
      ));
    }
    const saved = await api.put<AgentInputConfig>(
      ws(`/v1/config/agents/${agentId}/resources`),
      {
        agent_id: agentId,
        // Environment defaults have their own pinned revision and are not edited
        // by the resource list. Preserve the existing binding losslessly.
        environment: existingResources.data?.environment,
        inputs: resourceInputs,
        revision: resourceRevision + 1,
      },
    );
    setResourceRevision(saved.revision);
    setResourcesDirty(false);
    return saved.revision;
  };
  const review = useAgentDraftReview({
    agentId: targetId(),
    config: cfg,
    dirty,
    canSave,
    readyModels: models.length,
    blank: BLANK,
    setConfig: setCfg,
    setDirty,
    onIssues: setIssues,
    onOpenPublish: () => setShowPublish(true),
    saveConfig: async () => { await saveConfig(); },
    saveResources: async () => { await saveResources(); },
    onError: (error) => toast.err(error instanceof Error ? error.message : "error"),
  });

  useEffect(() => {
    if (previousRouteId.current === id) return;
    previousRouteId.current = id;
    const requestedStage = searchParams.get("stage");
    const requestedSection = searchParams.get("section");
    setStage(requestedStage === "build" || requestedStage === "advanced" ? requestedStage : "quickstart");
    setBuilderSection(["instructions", "tools", "integrations", "knowledge"].includes(requestedSection ?? "")
      ? requestedSection as BuilderSection
      : "instructions");
    setAdvancedSection(["orchestration", "extensions", "source", "release"].includes(requestedSection ?? "")
      ? requestedSection as AdvancedSection
      : "orchestration");
    setIssues([]);
    setRawValid(true);
    setIntegrationsValid(true);
    setResourceInputs([]);
    setResourceRevision(0);
    setResourcesDirty(false);
    setPermissionPreset(null);
    importedMemory.current = "";
    attachedAuxiliary.current = "";
    review.reset();
    if (id === "new") {
      setCfg(BLANK);
      setDirty(false);
    }
  }, [id, review, searchParams]);

  useEffect(() => {
    if (!existing.data) return;
    setPermissionPreset(null);
    const { published: _published, ...rest } = existing.data;
    const auxiliary = Boolean(rest.metadata?.[AUXILIARY_PARENT_KEY]);
    const needsDefaultAuxiliary = !auxiliary && rest.multiagent == null;
    setCfg({
      ...BLANK,
      ...rest,
      multiagent: auxiliary ? rest.multiagent : rest.multiagent ?? BLANK.multiagent,
      delegation_limits: auxiliary
        ? rest.delegation_limits
        : rest.delegation_limits ?? BLANK.delegation_limits,
    });
    setDirty(needsDefaultAuxiliary);
    review.reset();
    setIntegrationsValid(true);
  // Review reset is intentionally tied to a newly hydrated server Draft.
  // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [existing.data]);
  useEffect(() => {
    if (!isNew || !parentAgentId) return;
    setCfg((current) => ({
      ...current,
      name: current.name || app.t("Specialist", "专属辅助 Agent"),
      description: current.description || app.t(
        `Attached specialist owned by ${parentAgentId}.`,
        `归属于 ${parentAgentId} 的专属辅助 Agent。`,
      ),
      metadata: {
        ...current.metadata,
        [AUXILIARY_PARENT_KEY]: parentAgentId,
        [AUXILIARY_ROLE_KEY]: "auxiliary",
      },
      multiagent: undefined,
      delegation_limits: undefined,
    }));
    setDirty(true);
  }, [app, isNew, parentAgentId]);
  useEffect(() => {
    if (!attachAuxiliaryId || !existing.data || attachedAuxiliary.current === attachAuxiliaryId) return;
    setCfg((current) => {
      const roster = current.multiagent?.agents ?? [{ type: "self" } as const];
      if (roster.some((target) => delegateTargetView(target, current.id).id === attachAuxiliaryId)) return current;
      const self = roster.filter((target) => typeof target !== "string" && target.type === "self");
      const specialists = roster.filter((target) => typeof target === "string" || target.type !== "self");
      return { ...current, multiagent: withRoster([...specialists, agentTarget(attachAuxiliaryId), ...self]) };
    });
    setDirty(true);
    setStage("advanced");
    setAdvancedSection("orchestration");
    attachedAuxiliary.current = attachAuxiliaryId;
    toast.info(app.t(
      "Published specialist added to this Agent's draft. Review and publish the parent to activate it.",
      "已发布的专属辅助 Agent 已加入当前主 Agent 草稿；请审阅并发布主 Agent 以启用。",
    ));
  }, [app, attachAuxiliaryId, existing.data, toast]);
  useEffect(() => {
    if (!isNew || searchParams.get("template") !== "dream") return;
    setCfg((current) => current.id ? current : {
      ...current,
      id: "awaken_builtin_dream_agent",
      name: "Dream Agent",
      description: "System Agent used to curate durable memories from frozen Session evidence.",
      system: "Curate durable, evidence-backed memory. Preserve newer facts, merge duplicates, and never infer secrets.",
      metadata: { ...current.metadata, "awaken.system_agent": "dream" },
    });
    setDirty(true);
  }, [isNew, searchParams]);
  useEffect(() => {
    if (existingResources.data && !resourcesDirty) {
      setResourceInputs(existingResources.data.inputs);
      setResourceRevision(existingResources.data.revision);
    }
  }, [existingResources.data, resourcesDirty]);
  useEffect(() => {
    const memoryStoreId = searchParams.get("memory_store") ?? "";
    if (!memoryStoreId || importedMemory.current === memoryStoreId) return;
    if (!isNew && existingResources.isLoading) return;
    if (resourceInputs.some((binding) => binding.target.kind === "memory_store" && binding.target.id === memoryStoreId)) {
      importedMemory.current = memoryStoreId;
      return;
    }
    const safeId = memoryStoreId.replace(/[^a-zA-Z0-9_-]/g, "-");
    setResourceInputs((current) => [...current, {
      binding_id: `dream-output-${safeId}`,
      target: { kind: "memory_store", id: memoryStoreId },
      mount_path: `/mnt/memory/${safeId}`,
      access: "read_write",
      instructions: "Review and use this Dream output as curated durable memory.",
    }]);
    setResourcesDirty(true);
    setCfg((current) => current.plugins.includes("memory") ? current : { ...current, plugins: [...current.plugins, "memory"] });
    setDirty(true);
    setStage("build");
    setBuilderSection("knowledge");
    importedMemory.current = memoryStoreId;
    toast.ok(app.t("Dream output added to the Agent resource draft.", "Dream 输出已加入 Agent 资源草稿。"));
  }, [app, existingResources.isLoading, isNew, resourceInputs, searchParams, toast]);
  useEffect(() => {
    if (savedId && !dirty) {
      nav(`/w/${wsId}/agents/${savedId}${parentAgentId ? `?parent=${encodeURIComponent(parentAgentId)}` : ""}`, { replace: true });
      setSavedId(null);
    }
  }, [savedId, dirty, wsId, nav, parentAgentId]);
  useEffect(() => {
    if (quickRunSessionId && !dirty && !resourcesDirty) {
      nav(quickstartSessionPath(wsId, quickRunSessionId));
      setQuickRunSessionId(null);
    }
  }, [quickRunSessionId, dirty, resourcesDirty, wsId, nav]);

  const baseline = useMemo(() => {
    if (!existing.data) return {};
    const { published: _published, ...rest } = existing.data;
    return rest;
  }, [existing.data]);
  const patch = (value: Partial<AgentConfig>) => {
    setCfg((current) => ({ ...current, ...value }));
    if (["tools", "plugins", "plugin_config"].some((key) => key in value)) {
      setPermissionPreset(null);
    }
    setDirty(true);
    review.onManualEdit(Object.keys(value));
    if (issues.length) setIssues([]);
  };
  const replaceRaw = (value: AgentConfig) => {
    setCfg({ ...BLANK, ...value });
    setPermissionPreset(null);
    setDirty(true);
    review.onManualEdit(Object.keys(value));
    setIntegrationsValid(true);
    if (issues.length) setIssues([]);
  };
  const routeIssue = (path: string) => {
    const nextStage = stageForPath(path);
    setStage(nextStage);
    if (nextStage === "build") setBuilderSection(builderSectionForPath(path));
    else setAdvancedSection(advancedSectionForPath(path));
  };

  const validate = useMutation({
    mutationFn: () => api.post<ValidationResult>(
      ws(`/v1/config/agents/${targetId()}/validate`),
      body(),
    ),
    onSuccess: (result) => {
      setIssues(result.issues ?? []);
      review.setValidationResult(result.valid);
      if (result.valid) toast.ok(app.t("Config is valid.", "配置有效。"));
      else toast.err(app.t("Config has issues — see below.", "配置有问题，请查看下方。"));
    },
    onError: (error) => toast.err(error instanceof Error ? error.message : "error"),
  });
  const save = useMutation({
    mutationFn: async () => {
      await saveConfig();
      await saveResources();
    },
    onSuccess: () => {
      setDirty(false);
      setResourcesDirty(false);
      review.markSaved();
      toast.ok(app.t("Saved.", "已保存。"));
      void qc.invalidateQueries({ queryKey: ["config-agents"] });
      void qc.invalidateQueries({ queryKey: ["config-agent", wsId, targetId()] });
      if (isNew) setSavedId(targetId());
    },
    onError: (error) => toast.err(error instanceof Error ? error.message : "error"),
  });
  const publish = useMutation({
    mutationFn: () => api.post<PublishResult>(
      ws(`/v1/config/agents/${targetId()}/publish`),
      {
        source_revision: cfg.generation,
        resource_revision: resourceRevision,
      },
    ),
    onSuccess: (result) => {
      toast.ok(app.t(
        `Published · ${result.fingerprint.slice(0, 12)}`,
        `已发布 · ${result.fingerprint.slice(0, 12)}`,
      ));
      void qc.invalidateQueries({ queryKey: ["config-agents"] });
      void qc.invalidateQueries({ queryKey: ["config-agent", wsId, id] });
      review.markReady();
      if (parentAgentId) {
        nav(`/w/${wsId}/agents/${parentAgentId}?stage=advanced&section=orchestration&attach=${encodeURIComponent(targetId())}`, { replace: true });
      } else if (isNew) nav(`/w/${wsId}/agents/${targetId()}`, { replace: true });
    },
    onError: (error) => toast.err(error instanceof Error ? error.message : "error"),
  });
  const quickRun = useMutation({
    mutationFn: async (intent: QuickRunIntent) => {
      const sourceRevision = await saveConfig();
      const reviewedResourceRevision = await saveResources();
      const validation = await api.post<ValidationResult>(
        ws(`/v1/config/agents/${targetId()}/validate`),
        body(),
      );
      setIssues(validation.issues ?? []);
      if (!validation.valid) {
        throw new Error(app.t(
          "The draft needs attention before its first run.",
          "首次运行前需要先处理草稿中的问题。",
        ));
      }
      const publication = await api.post<PublishResult>(
        ws(`/v1/config/agents/${targetId()}/publish`),
        {
          source_revision: sourceRevision,
          resource_revision: reviewedResourceRevision,
        },
      );
      const request: CreateSessionRequest = {
        // Publish is the immutable execution boundary. Pin the Session to the
        // exact revision acknowledged by that boundary so an active-active
        // Coordinator can either load that publication or fail creation
        // closed; it must never run an unversioned fallback configuration.
        agent: { id: targetId(), type: "agent", version: publication.source_revision },
        environment_id: intent.environmentId ?? BUILTIN_LOCAL_ENVIRONMENT_ID,
        // The official create boundary owns initial_events atomically. A
        // second POST would create a partial Session on transport failure and
        // duplicate idempotency/retry semantics already owned by creation.
        initial_events: [{
          type: "user.message",
          content: [{ type: "text", text: intent.task }],
        }],
        title: app.t("Quickstart first run", "Quickstart 首次运行"),
      };
      return createManagedSession(
        request,
        quickRunCreateIdentity.current,
      );
    },
    onSuccess: (session) => {
      quickRunCreateIdentity.current.complete();
      setDirty(false);
      setResourcesDirty(false);
      setQuickRunIntent(undefined);
      setQuickRunSessionId(session.id);
      review.markReady();
      void qc.invalidateQueries({ queryKey: ["config-agents"] });
      void qc.invalidateQueries({ queryKey: ["sessions", wsId] });
      toast.ok(app.t(
        "Published and started a real Session.",
        "已发布并启动真实 Session。",
      ));
    },
    onError: (error) => {
      const message = error instanceof Error ? error.message : String(error);
      toast.err(message);
    },
  });

  const changed = (path: string) => review.changedPaths.some((candidate) =>
    candidate === path
      || candidate.startsWith(`${path}.`)
      || path.startsWith(`${candidate}.`));
  const stageChanged = (candidate: AuthorStage) => review.changedPaths.some((path) =>
    stageForPath(path) === candidate);
  const resourcesError = existingResources.error instanceof Error
    && !isAbsent(existingResources.error)
    ? existingResources.error
    : undefined;

  return (
    <>
      <AgentEditorHeader
        id={id}
        name={cfg.name}
        isNew={isNew}
        dirty={hasUnsavedChanges}
        status={review.status}
        canSave={canSave}
        validatePending={validate.isPending}
        savePending={save.isPending}
        publishPending={publish.isPending}
        onBack={() => nav(parentAgentId
          ? `/w/${wsId}/agents/${parentAgentId}?stage=advanced&section=orchestration`
          : `/w/${wsId}/agents`)}
        onValidate={() => validate.mutate()}
        onSave={() => save.mutate()}
        onPublish={() => void review.preparePublish(publish.isPending)}
      />
      <AttachedAgentContext
        parentId={parentAgentId}
        onBack={() => nav(`/w/${wsId}/agents/${parentAgentId}?stage=advanced&section=orchestration`)}
      />
      <ReadinessPanel compact />

      <AgentEditorStageNavigation
        stage={stage}
        changed={stageChanged}
        onChange={setStage}
        canTry={managedRuntime}
        onTry={() => setShowSandbox(true)}
      />
      <AgentValidationIssues issues={issues} onOpen={routeIssue} />

      <div className="agent-editor">
        <AgentEditorStages
          stage={stage} builderSection={builderSection} advancedSection={advancedSection}
          config={cfg} baseline={baseline} resources={resourceInputs} resourcesError={resourcesError}
          resourceInputDefaults={caps.data?.resource_inputs?.default_mounts}
          resourceRevision={resourceRevision} isNew={isNew} published={existing.data?.published === true}
          canRun={managedRuntime && canSave && modelIsRunnable} runPending={quickRun.isPending} publishPending={publish.isPending}
          readyModels={models} allModels={allModels} runtimes={caps.data?.runtimes ?? []}
          tools={caps.data?.tools ?? []} plugins={caps.data?.plugins ?? []}
          toolsets={caps.data?.toolsets ?? []}
          credentials={credentials.data ?? []} changed={changed} onPatch={patch} onRawChange={replaceRaw}
          onApplyControlledModifications={() => { setPermissionPreset("controlled_modifications"); setDirty(true); }}
          onManageModels={() => setManageModels(true)}
          onReviewRun={(environmentId, task) => { quickRun.reset(); setQuickRunIntent({ environmentId, task }); }}
          onBuilderSectionChange={setBuilderSection} onAdvancedSectionChange={setAdvancedSection}
          onResourcesChange={(inputs) => {
            const memoryPatch = reconcileMemoryBinding(resourceInputs, inputs, cfg);
            if (Object.keys(memoryPatch).length > 0) patch(memoryPatch);
            if (shouldEnableMemoryExtraction(resourceInputs, inputs, cfg.plugins)) {
              toast.info(app.t("Memory settings enabled for the newly bound store.", "已为新绑定的记忆库启用 Memory 设置。"));
            }
            setResourceInputs(inputs); setResourcesDirty(true); review.onManualEdit(["resources"]);
          }}
          onRetryResources={() => void existingResources.refetch()}
          onIntegrationsValidityChange={setIntegrationsValid} onRawValidityChange={setRawValid}
          onPublish={() => void review.preparePublish(publish.isPending)}
        />
      </div>

      {manageModels && (
        <Drawer title={app.t("Models", "模型")} onClose={() => setManageModels(false)}>
          <ModelsSurface />
        </Drawer>
      )}
      {managedRuntime && showSandbox && (
        <Drawer title={app.t("Try current draft", "试运行当前草稿")} onClose={() => setShowSandbox(false)}>
          <SandboxPane
            draft={body()}
            resources={resourceInputs}
            canPreview={canSave && modelIsRunnable}
          />
        </Drawer>
      )}

      <AgentPublicationModals
        config={body()}
        baseline={baseline}
        resources={resourceInputs}
        resourceRevision={resourceRevision}
        quickRunIntent={quickRunIntent}
        quickRunPending={quickRun.isPending}
        quickRunError={quickRun.error instanceof Error ? quickRun.error : undefined}
        showPublish={showPublish}
        publishPending={publish.isPending}
        onCloseQuickRun={() => setQuickRunIntent(undefined)}
        onConfirmQuickRun={(intent) => quickRun.mutate(intent)}
        onClosePublish={() => setShowPublish(false)}
        onConfirmPublish={() => {
          publish.mutate();
          setShowPublish(false);
        }}
      />
    </>
  );
}
