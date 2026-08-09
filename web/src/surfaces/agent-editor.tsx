// Agent authoring is progressively disclosed as Quickstart → Build → Advanced.
// The three stages edit one lossless draft; Save/Validate/Try/Publish remain global.
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useEffect, useMemo, useState } from "react";
import { useNavigate, useParams, useSearchParams } from "react-router";
import AgentAdvanced from "../components/agent/AgentAdvanced";
import AgentBuilder from "../components/agent/AgentBuilder";
import AgentEditorHeader from "../components/agent/AgentEditorHeader";
import AgentPublicationModals, {
  type QuickRunIntent,
} from "../components/agent/AgentPublicationModals";
import AgentQuickstart from "../components/agent/AgentQuickstart";
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
import { Button } from "../components/ui";
import { useToast } from "../components/ui/Toast";
import {
  api,
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
  Session,
  ValidationIssue,
  ValidationResult,
} from "../lib/api/types";
import { labelForPath } from "../lib/config-diff";
import { shouldEnableMemoryExtraction } from "../lib/agent-memory-binding";
import { useApp } from "../lib/app-state";
import { useCapabilities } from "../lib/useCapabilities";
import { useModels } from "../lib/useModels";
import { useUnsavedGuard } from "../lib/useUnsavedGuard";
import ModelsSurface from "./models";

const BLANK: AgentConfig = {
  id: "",
  name: "",
  model: { mode: "auto" },
  system: "You are a helpful coding agent.",
  metadata: {},
  tools: [],
  mcp_servers: [],
  skills: [],
  max_steps: 8,
  plugins: [],
  plugin_config: {},
  context_policy: { kind: "keep_all" },
};

export default function AgentEditorSurface() {
  const app = useApp();
  const nav = useNavigate();
  const qc = useQueryClient();
  const toast = useToast();
  const { ws: wsId = "default", id = "new" } = useParams();
  const [searchParams] = useSearchParams();
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
  const [dirty, setDirty] = useState(false);
  const [resourceInputs, setResourceInputs] = useState<InputBinding[]>([]);
  const [resourceRevision, setResourceRevision] = useState(0);
  const [resourcesDirty, setResourcesDirty] = useState(false);
  const [issues, setIssues] = useState<ValidationIssue[]>([]);
  const [manageModels, setManageModels] = useState(false);
  const [savedId, setSavedId] = useState<string | null>(null);
  const [quickRunSessionId, setQuickRunSessionId] = useState<string | null>(null);
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
  const body = () => ({ ...cfg, id: targetId() });

  const modelIsRunnable = useMemo(() => {
    if (typeof cfg.model === "string") return models.includes(cfg.model);
    if ("id" in cfg.model) return models.includes(cfg.model.id);
    if (cfg.model.mode === "backend_default" || cfg.model.mode === "backend_exact") {
      const backendRef = cfg.model.backend_ref;
      return (caps.data?.runtimes ?? []).some((runtime) =>
        runtime.id === backendRef && runtime.local?.detected !== false);
    }
    return models.length > 0;
  }, [cfg.model, models, caps.data?.runtimes]);

  const saveConfig = async (): Promise<number | undefined> => {
    const result = await api.put<{ id: string; generation?: number }>(
      ws(`/v1/config/agents/${targetId()}`),
      body(),
    );
    if (result.generation !== undefined) {
      setCfg((current) => ({ ...current, generation: result.generation }));
    }
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
    if (!existing.data) return;
    const { published: _published, ...rest } = existing.data;
    setCfg({ ...BLANK, ...rest });
    setDirty(false);
    review.reset();
    setIntegrationsValid(true);
  // Review reset is intentionally tied to a newly hydrated server Draft.
  // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [existing.data]);
  useEffect(() => {
    if (existingResources.data && !resourcesDirty) {
      setResourceInputs(existingResources.data.inputs);
      setResourceRevision(existingResources.data.revision);
    }
  }, [existingResources.data, resourcesDirty]);
  useEffect(() => {
    if (savedId && !dirty) {
      nav(`/w/${wsId}/agents/${savedId}`, { replace: true });
      setSavedId(null);
    }
  }, [savedId, dirty, wsId, nav]);
  useEffect(() => {
    if (quickRunSessionId && !dirty && !resourcesDirty) {
      nav(`/w/${wsId}/sessions/${quickRunSessionId}`);
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
    setDirty(true);
    review.onManualEdit(Object.keys(value));
    if (issues.length) setIssues([]);
  };
  const replaceRaw = (value: AgentConfig) => {
    setCfg({ ...BLANK, ...value });
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
      if (isNew) nav(`/w/${wsId}/agents/${targetId()}`, { replace: true });
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
      await api.post<PublishResult>(
        ws(`/v1/config/agents/${targetId()}/publish`),
        {
          source_revision: sourceRevision,
          resource_revision: reviewedResourceRevision,
        },
      );
      const request: CreateSessionRequest = {
        agent: targetId(),
        environment_id: intent.environmentId,
        title: app.t("Quickstart first run", "Quickstart 首次运行"),
      };
      const session = await api.post<Session>(ws("/v1/sessions"), request);
      await api.post(ws(`/v1/sessions/${session.id}/events`), {
        events: [{
          type: "user.message",
          content: [{ type: "text", text: intent.task }],
        }],
      });
      return session;
    },
    onSuccess: (session) => {
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
  const stages: Array<{ key: AuthorStage; label: string; zh: string; description: string; descriptionZh: string }> = [
    {
      key: "quickstart",
      label: "Quickstart",
      zh: "快速开始",
      description: "Template to first real run",
      descriptionZh: "从模板到首次真实运行",
    },
    {
      key: "build",
      label: "Build",
      zh: "构建",
      description: "Prompt, capabilities and knowledge",
      descriptionZh: "提示词、能力与知识",
    },
    {
      key: "advanced",
      label: "Advanced",
      zh: "高级",
      description: "Orchestration, plugins and source",
      descriptionZh: "编排、Plugin 与原始配置",
    },
  ];
  const resourcesError = existingResources.error instanceof Error
    && !isAbsent(existingResources.error)
    ? existingResources.error
    : undefined;

  return (
    <>
      <AgentEditorHeader
        id={id}
        isNew={isNew}
        dirty={hasUnsavedChanges}
        status={review.status}
        canSave={canSave}
        validatePending={validate.isPending}
        savePending={save.isPending}
        publishPending={publish.isPending}
        onBack={() => nav(`/w/${wsId}/agents`)}
        onValidate={() => validate.mutate()}
        onSave={() => save.mutate()}
        onPublish={() => void review.preparePublish(publish.isPending)}
      />
      <ReadinessPanel compact />

      <div className="author-stage-nav" role="tablist" aria-label={app.t("Authoring stages", "创作阶段")}>
        {stages.map((item, index) => (
          <button
            key={item.key}
            className="author-stage-button"
            role="tab"
            aria-label={app.t(item.label, item.zh)}
            aria-selected={stage === item.key}
            data-active={stage === item.key}
            onClick={() => setStage(item.key)}
          >
            <span className="author-stage-index">{index + 1}</span>
            <span>
              <strong>{app.t(item.label, item.zh)}</strong>
              <small>{app.t(item.description, item.descriptionZh)}</small>
            </span>
            {stageChanged(item.key) && <span className="agent-change-dot">✦</span>}
          </button>
        ))}
        <Button variant="ghost" onClick={() => setShowSandbox(true)}>
          ▷ {app.t("Try draft", "试运行草稿")}
        </Button>
      </div>

      {issues.length > 0 && (
        <div className="banner warn issue-banner">
          {issues.map((issue, index) => (
            <div className="row" key={`${issue.path}-${index}`} style={{ justifyContent: "space-between" }}>
              <span>
                <strong>{labelForPath(issue.path) || app.t("Config", "配置")}</strong>
                {" — "}
                {issue.message}
              </span>
              <Button variant="ghost" onClick={() => routeIssue(issue.path)}>
                {app.t("Open field →", "打开对应字段 →")}
              </Button>
            </div>
          ))}
        </div>
      )}

      <div className="agent-editor">
        {stage === "quickstart" && (
          <AgentQuickstart
            config={cfg}
            readyModels={models}
            allModels={allModels}
            runtimes={caps.data?.runtimes ?? []}
            availableTools={(caps.data?.tools ?? []).map((tool) => tool.id)}
            availablePlugins={(caps.data?.plugins ?? []).map((plugin) => plugin.id)}
            canRun={canSave && modelIsRunnable}
            idEditable={isNew}
            published={existing.data?.published === true}
            runPending={quickRun.isPending}
            onPatch={patch}
            onManageModels={() => setManageModels(true)}
            onReviewRun={(environmentId, task) => {
              quickRun.reset();
              setQuickRunIntent({ environmentId, task });
            }}
          />
        )}
        {stage === "build" && (
          <AgentBuilder
            section={builderSection}
            config={cfg}
            isNew={isNew}
            readyModels={models}
            allModels={allModels}
            runtimes={caps.data?.runtimes ?? []}
            tools={caps.data?.tools ?? []}
            plugins={caps.data?.plugins ?? []}
            policies={caps.data?.policies ?? []}
            credentials={credentials.data ?? []}
            resources={resourceInputs}
            resourcesError={resourcesError}
            changed={changed}
            onSectionChange={setBuilderSection}
            onPatch={patch}
            onManageModels={() => setManageModels(true)}
            onResourcesChange={(inputs) => {
              if (shouldEnableMemoryExtraction(resourceInputs, inputs, cfg.plugins)) {
                patch({ plugins: [...cfg.plugins, "memory"] });
                toast.info(app.t(
                  "Memory settings enabled for the newly bound store.",
                  "已为新绑定的记忆库启用 Memory 设置。",
                ));
              }
              setResourceInputs(inputs);
              setResourcesDirty(true);
              review.onManualEdit(["resources"]);
            }}
            onRetryResources={() => void existingResources.refetch()}
            onValidityChange={setIntegrationsValid}
          />
        )}
        {stage === "advanced" && (
          <AgentAdvanced
            section={advancedSection}
            config={cfg}
            baseline={baseline}
            resources={resourceInputs}
            plugins={caps.data?.plugins ?? []}
            credentials={credentials.data ?? []}
            resourceRevision={resourceRevision}
            published={existing.data?.published === true}
            publishPending={publish.isPending}
            changed={changed}
            onSectionChange={setAdvancedSection}
            onPatch={patch}
            onRawChange={replaceRaw}
            onValidityChange={setRawValid}
            onPublish={() => void review.preparePublish(publish.isPending)}
          />
        )}
      </div>

      {manageModels && (
        <Drawer title={app.t("Models", "模型")} onClose={() => setManageModels(false)}>
          <ModelsSurface />
        </Drawer>
      )}
      {showSandbox && (
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
