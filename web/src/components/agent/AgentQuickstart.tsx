import { ArrowRight, Check, Circle, Info } from "@awaken/ui/icons";
import { InlineNotice, Button, Card } from "@awaken/ui";
import { useQuery } from "@tanstack/react-query";
import { useEffect, useRef, useState } from "react";
import {
    type AgentConfig,
    type AgentToolsetCap,
  type Environment,
  type Page,
  type RuntimeCap,
} from "../../lib/api/types";
import { api, BUILTIN_LOCAL_ENVIRONMENT_ID, ws } from "../../lib/api/client";
import { selectedAgentToolIds, withSelectedAgentTools } from "../../lib/agent-toolsets";
import {
  STARTERS,
  type StarterTemplate,
  starterAgentId,
  starterById,
  starterOutcomeDraft,
} from "../../lib/agent-starters";
import {
  type OutcomeDraft,
  emptyOutcomeDraft,
  outcomeDraftComplete,
} from "../../lib/session-outcome";
import { useApp } from "../../lib/app-state";
import AgentModelSelectionEditor from "./AgentModelSelectionEditor";
import OutcomeFields from "../session/OutcomeFields";
import QuickstartProgress, { type QuickstartStep } from "./QuickstartProgress";
import type { QuickRunIntent } from "./AgentPublicationModals";
import { Pill, TextAreaField, TextField } from "../ui";
import { entityDisplayName, identifierLabel } from "../../lib/presentation";

export { starterAgentId } from "../../lib/agent-starters";

export default function AgentQuickstart({
  config,
  readyModels,
  allModels,
  runtimes,
  availableTools,
  agentToolset,
  availablePlugins,
  canRun,
  idEditable,
  published,
  runPending,
  initialTemplateId,
  onPatch,
  onManageModels,
  onRefreshRuntimes,
  runtimeUpdatedAt,
  onReviewRun,
}: {
  config: AgentConfig;
  readyModels: string[];
  allModels: string[];
  runtimes: RuntimeCap[];
  availableTools: string[];
  agentToolset?: AgentToolsetCap;
  availablePlugins: string[];
  canRun: boolean;
  idEditable: boolean;
  published: boolean;
  runPending: boolean;
  /** The Starter the landing surface handed over, if any. Applied once so the
   * step opens already seeded; every field stays editable afterwards. */
  initialTemplateId?: string;
  onPatch: (patch: Partial<AgentConfig>) => void;
  onManageModels: () => void;
  onRefreshRuntimes?: () => void;
  runtimeUpdatedAt?: number;
  onReviewRun: (intent: QuickRunIntent) => void;
}) {
  const app = useApp();
  const agentToolsetMembers = agentToolset?.members ?? [];
  const [selectedTemplate, setSelectedTemplate] = useState("");
  const [environmentId, setEnvironmentId] = useState(BUILTIN_LOCAL_ENVIRONMENT_ID);
  const [task, setTask] = useState(app.t(
    "Introduce yourself in one sentence and explain how you would approach your configured role.",
    "用一句话介绍自己，并说明你会如何完成当前配置的职责。",
  ));
  // An Outcome is never implied. A Starter may propose one, which prefills the
  // draft and says so; sending `user.define_outcome` still needs this opt-in.
  const [outcomeEnabled, setOutcomeEnabled] = useState(false);
  const [outcomeProposed, setOutcomeProposed] = useState(false);
  const [outcomeDraft, setOutcomeDraft] = useState<OutcomeDraft>(emptyOutcomeDraft);
  const environments = useQuery({
    queryKey: ["environments", app.workspaceId],
    queryFn: () => api.get<Page<Environment>>(ws("/v1/environments")),
  });

  const applyTemplate = (template: StarterTemplate) => {
    setSelectedTemplate(template.id);
    const tools = template.suggestedTools.filter((id) => availableTools.includes(id));
    const selectedTools = selectedAgentToolIds(config.tools, agentToolsetMembers);
    const plugins = (template.suggestedPlugins ?? []).filter((id) => availablePlugins.includes(id));
    onPatch({
      id: starterAgentId(config.id, template.id),
      name: template.name,
      description: app.t(template.description, template.descriptionZh),
      system: template.system,
      tools: agentToolset
        ? withSelectedAgentTools(
            config.tools,
            Array.from(new Set([...selectedTools, ...tools])),
            agentToolset.members,
            agentToolset.default_config.permission_policy,
          )
        : config.tools,
      plugins: Array.from(new Set([...config.plugins, ...plugins])),
    });
    setTask(app.t(template.firstTask, template.firstTaskZh));
    const proposal = starterOutcomeDraft(template, app.t);
    setOutcomeProposed(proposal != null);
    setOutcomeDraft(proposal ?? emptyOutcomeDraft());
    if (!proposal) setOutcomeEnabled(false);
  };

  // The landing surface hands over a Starter by route, not by shared state.
  // Apply it exactly once so re-renders never overwrite later manual edits.
  const seededTemplate = useRef<string | undefined>(undefined);
  useEffect(() => {
    const starter = starterById(initialTemplateId);
    if (!starter || seededTemplate.current === starter.id) return;
    seededTemplate.current = starter.id;
    applyTemplate(starter);
  }, [initialTemplateId]);

  const outcomeReady = !outcomeEnabled || outcomeDraftComplete(outcomeDraft);
  const readiness = [
    {
      ready: Boolean(config.name?.trim()),
      label: app.t("Display name", "显示名称"),
      detail: config.name?.trim() || app.t("Required", "必填"),
    },
    {
      ready: config.id.trim().length > 0,
      label: app.t("Stable Agent ID", "稳定 Agent ID"),
      detail: config.id.trim() || app.t("Required", "必填"),
    },
    {
      ready: canRun,
      label: app.t("Runnable model", "可运行模型"),
      detail: canRun ? app.t("Ready", "就绪") : app.t("Choose a credentialed model", "选择已有凭证的模型"),
    },
    {
      ready: task.trim().length > 0,
      label: app.t("First task", "首次任务"),
      detail: task.trim() ? app.t("Ready", "就绪") : app.t("Required", "必填"),
    },
    // Only an Outcome the operator opted into can block the run; an absent one
    // is the ordinary case, not an unmet requirement.
    ...(outcomeEnabled ? [{
      ready: outcomeDraftComplete(outcomeDraft),
      label: app.t("Outcome", "结果标准"),
      detail: outcomeDraftComplete(outcomeDraft)
        ? app.t(`At most ${outcomeDraft.maxIterations} revisions`, `最多 ${outcomeDraft.maxIterations} 轮修订`)
        : app.t("Deliverable and rubric required", "需要填写交付物与评分标准"),
    }] : []),
  ];
  const allReady = readiness.every((item) => item.ready);

  // The rail is derived, never stored: the operator is on the Session step once
  // the Environment is chosen and a task exists, and on step 2 once the Agent
  // identity and model are settled.
  const currentStep: QuickstartStep = !(config.name?.trim() && config.id.trim() && canRun)
    ? "agent"
    : task.trim()
      ? "session"
      : "environment";

  return (
    <div className="quickstart-layout">
      <div className="stack">
        <QuickstartProgress current={currentStep} />
        <div>
          <h2 className="section-title">{app.t("1 · Create the Agent", "1 · 创建 Agent")}</h2>
          <p className="hint">{app.t(
            "Templates update this draft's prompt and suggested capabilities. Advanced configuration is preserved.",
            "模板会更新当前草稿的提示词和建议能力，高级配置保持不变。",
          )}</p>
        </div>
        <div className="template-grid">
          {STARTERS.map((template) => (
            <button
              key={template.id}
              className="template-card"
              data-selected={selectedTemplate === template.id}
              onClick={() => applyTemplate(template)}
            >
              <strong>{app.t(template.title, template.titleZh)}</strong>
              <span>{app.t(template.description, template.descriptionZh)}</span>
              <small className="mut">{app.t(template.requirement, template.requirementZh)}</small>
              {template.outcome && (
                <Pill tone="info">{app.t("Outcome-checked", "结果自评")}</Pill>
              )}
            </button>
          ))}
        </div>
        <Card className="quickstart-step">
          <TextField
            label={app.t("Display name", "显示名称")}
            hint={app.t("The human-readable name shown throughout the Console.", "在 Console 各处显示的人类可读名称。")}
            placeholder={app.t("Release Readiness Reviewer", "发布就绪审查 Agent")}
            value={config.name ?? ""}
            onChange={(event) => onPatch({ name: event.target.value })}
          />
          <TextField
            label={app.t("Agent ID", "Agent ID")}
            hint={app.t("A stable identifier used by APIs and Sessions. It cannot be changed after creation.", "供 API 和会话使用的稳定标识；创建后不能修改。")}
            mono
            placeholder="coding-agent"
            value={config.id}
            disabled={!idEditable}
            onChange={(event) => onPatch({ id: event.target.value })}
          />
          <AgentModelSelectionEditor
            model={config.model}
            readyModels={readyModels}
            allModels={allModels}
            runtimes={runtimes}
            onRefreshRuntimes={onRefreshRuntimes}
            runtimeUpdatedAt={runtimeUpdatedAt}
            onChange={(model) => onPatch({ model })}
            onManage={onManageModels}
          />
        </Card>

        <Card className="quickstart-step">
          <h2 className="section-title">{app.t("2 · Configure where the first run executes", "2 · 配置首次运行环境")}</h2>
          <label className="ui-field">
            <span>{app.t("Environment for this run", "本次运行的 Environment")}</span>
            <select
              className="ui-input"
              aria-label={app.t("Environment for this run", "本次运行的 Environment")}
              value={environmentId}
              onChange={(event) => setEnvironmentId(event.target.value)}
            >
              <option value={BUILTIN_LOCAL_ENVIRONMENT_ID}>{app.t("Local · built-in", "本地 · 内置")}</option>
              {(environments.data?.data ?? []).filter((environment) => (
                environment.id !== BUILTIN_LOCAL_ENVIRONMENT_ID
              )).map((environment) => (
                <option key={environment.id} value={environment.id}>
                  {entityDisplayName(environment.name, identifierLabel(environment.id))}
                </option>
              ))}
            </select>
          </label>
          <span className="mut">{app.t(
            "This choice applies to the first run only. You can choose another Environment when creating later Sessions.",
            "此选择只用于首次运行；以后创建会话时可以选择其他运行环境。",
          )}</span>
          {environments.error instanceof Error && <div className="err">{environments.error.message}</div>}
        </Card>

        <Card className="quickstart-step">
          <h2 className="section-title">{app.t("3 · Start the Session", "3 · 启动会话")}</h2>
          <TextAreaField
            label={app.t("Task sent to the new Session", "发送到新 Session 的任务")}
            rows={4}
            value={task}
            onChange={(event) => setTask(event.target.value)}
          />
          <label className="quickstart-outcome-toggle">
            <input
              type="checkbox"
              checked={outcomeEnabled}
              onChange={(event) => setOutcomeEnabled(event.target.checked)}
            />
            <span>
              {app.t(
                "Have the Agent self-evaluate against a rubric until it passes (optional)",
                "让 Agent 对照评分标准自评并修订，直到达标（可选）",
              )}
              {outcomeProposed && !outcomeEnabled && (
                <> · <span className="mut">{app.t("this template proposes one", "此模板已建议一份")}</span></>
              )}
            </span>
          </label>
          {outcomeEnabled && (
            <OutcomeFields draft={outcomeDraft} onChange={setOutcomeDraft} />
          )}
        </Card>
      </div>

      <aside className="quickstart-summary">
        <Card>
          <div className="row" style={{ justifyContent: "space-between" }}>
            <strong>{app.t("Ready to run", "运行就绪")}</strong>
            <Pill tone={allReady ? "ok" : "warn"}>
              {readiness.filter((item) => item.ready).length}/{readiness.length}
            </Pill>
          </div>
          <div className="readiness-list">
            {readiness.map((item) => (
              <div className="readiness-item" key={item.label} data-ready={item.ready}>
                <span>{item.ready ? <Check /> : <Circle />}</span>
                <span>
                  <strong>{item.label}</strong>
                  <small>{item.detail}</small>
                </span>
              </div>
            ))}
          </div>
          <InlineNotice tone="info" icon={<Info />} style={{ marginTop: 12 }}>
            <span>{published
              ? app.t("This will publish the reviewed draft and create a new durable Session.", "将发布已审阅草稿并创建新的持久 Session。")
              : app.t("You will review the publication diff before the Agent becomes runnable.", "Agent 可运行前会先展示发布差异供你确认。")}</span>
          </InlineNotice>
          <Button
            variant="primary"
            style={{ width: "100%", marginTop: 12, justifyContent: "center" }}
            disabled={!allReady || runPending}
            onClick={() => onReviewRun({
              environmentId,
              task: task.trim(),
              outcome: outcomeEnabled && outcomeReady ? outcomeDraft : undefined,
            })}
          >
            {runPending ? app.t("Starting…", "正在启动…") : app.t("Review, publish & run", "审阅、发布并运行")} <ArrowRight />
          </Button>
          <p className="hint" style={{ marginTop: 10 }}>{app.t(
            "Step 4 opens on the Session that starts here: connect the same Agent from your own backend.",
            "第 4 步会在这里启动的 Session 上继续：从你自己的后端接入同一个 Agent。",
          )}</p>
        </Card>
      </aside>
    </div>
  );
}
