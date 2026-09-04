import { useQuery } from "@tanstack/react-query";
import { useState } from "react";
import {
    type AgentConfig,
    type AgentToolsetCap,
  type Environment,
  type Page,
  type RuntimeCap,
} from "../../lib/api/types";
import { api, BUILTIN_LOCAL_ENVIRONMENT_ID, ws } from "../../lib/api/client";
import { selectedAgentToolIds, withSelectedAgentTools } from "../../lib/agent-toolsets";
import { useApp } from "../../lib/app-state";
import AgentModelSelectionEditor from "./AgentModelSelectionEditor";
import { Button, Card, Pill, TextAreaField, TextField } from "../ui";
import { entityDisplayName, identifierLabel } from "../../lib/presentation";

interface StarterTemplate {
  id: string;
  title: string;
  titleZh: string;
  description: string;
  descriptionZh: string;
  name: string;
  system: string;
  maxSteps: number;
  firstTask: string;
  firstTaskZh: string;
  requirement: string;
  requirementZh: string;
  suggestedTools: string[];
  suggestedPlugins?: string[];
}

const STARTERS: StarterTemplate[] = [
  {
    id: "task-assistant",
    title: "Task assistant",
    titleZh: "任务助手",
    description: "Turn one clearly-scoped request into a checked result.",
    descriptionZh: "把一项边界清晰的请求转化为经过检查的结果。",
    name: "Task Assistant",
    system: "You are a focused task assistant. Clarify the intended outcome from available context, complete the work with the configured capabilities, verify the result, and report material limitations.",
    maxSteps: 10,
    firstTask: "Summarize your role, list the inputs you need, and complete one small representative task.",
    firstTaskZh: "概括你的职责、列出所需输入，并完成一项小型代表任务。",
    requirement: "No Sandbox unless a selected Tool or Skill needs one",
    requirementZh: "仅在所选 Tool 或 Skill 需要时创建 Sandbox",
    suggestedTools: [],
  },
  {
    id: "repository-change",
    title: "Repository change",
    titleZh: "代码仓改动",
    description: "Inspect a repository, make a focused change, and run its checks.",
    descriptionZh: "检查代码仓、完成聚焦改动并运行验证。",
    name: "Repository Change Agent",
    system: "You are a careful coding agent. Inspect the repository, make focused changes, and verify behavior before reporting completion.",
    maxSteps: 16,
    firstTask: "Inspect the mounted repository, identify its verification commands, and report the smallest safe first change.",
    firstTaskZh: "检查已挂载代码仓，找出验证命令，并说明最小且安全的首个改动。",
    requirement: "Requires a filesystem-capable Environment / Sandbox",
    requirementZh: "需要具备文件系统能力的 Environment / Sandbox",
    suggestedTools: ["bash", "read", "write", "glob", "grep"],
  },
  {
    id: "evidence-brief",
    title: "Evidence brief",
    titleZh: "证据简报",
    description: "Answer a decision question with traceable sources and uncertainty.",
    descriptionZh: "用可追溯来源与不确定性说明回答一个决策问题。",
    name: "Evidence Brief Agent",
    system: "You are a research agent. Gather evidence, separate observation from inference, and produce a concise sourced synthesis.",
    maxSteps: 12,
    firstTask: "Produce a short evidence brief on the supplied question, separating verified facts, inference, and open gaps.",
    firstTaskZh: "围绕给定问题生成简短证据简报，区分已验证事实、推断与待补信息。",
    requirement: "Requires a web/search Tool, MCP server, or supplied Files",
    requirementZh: "需要 Web/Search Tool、MCP Server 或已提供 Files",
    suggestedTools: ["web_search"],
    suggestedPlugins: ["web_search"],
  },
];

/** A Starter may remove the machine-id chore from a blank draft, but it must
 * never overwrite an id the operator already chose. */
export function starterAgentId(currentId: string, starterId: string): string {
  return currentId.trim() ? currentId : starterId;
}

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
  onPatch: (patch: Partial<AgentConfig>) => void;
  onManageModels: () => void;
  onRefreshRuntimes?: () => void;
  runtimeUpdatedAt?: number;
  onReviewRun: (environmentId: string, task: string) => void;
}) {
  const app = useApp();
  const agentToolsetMembers = agentToolset?.members ?? [];
  const [selectedTemplate, setSelectedTemplate] = useState("");
  const [environmentId, setEnvironmentId] = useState(BUILTIN_LOCAL_ENVIRONMENT_ID);
  const [task, setTask] = useState(app.t(
    "Introduce yourself in one sentence and explain how you would approach your configured role.",
    "用一句话介绍自己，并说明你会如何完成当前配置的职责。",
  ));
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
      max_steps: template.maxSteps,
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
  };

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
  ];

  return (
    <div className="quickstart-layout">
      <div className="stack">
        <div>
          <h2 className="section-title">{app.t("1 · Choose a starting point", "1 · 选择起点")}</h2>
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
            </button>
          ))}
        </div>
        <Card className="quickstart-step">
          <h2 className="section-title">{app.t("2 · Name and identify this Agent", "2 · 为 Agent 命名并设置标识")}</h2>
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
        </Card>

        <Card className="quickstart-step">
          <h2 className="section-title">{app.t("3 · Choose a runnable model", "3 · 选择可运行模型")}</h2>
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
          <h2 className="section-title">{app.t("4 · Choose where the first run executes", "4 · 选择首次运行环境")}</h2>
          <label className="field">
            <span>{app.t("Environment for this run", "本次运行的 Environment")}</span>
            <select
              className="input"
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
          <h2 className="section-title">{app.t("5 · Define the first real task", "5 · 定义首次真实任务")}</h2>
          <TextAreaField
            label={app.t("Task sent to the new Session", "发送到新 Session 的任务")}
            rows={4}
            value={task}
            onChange={(event) => setTask(event.target.value)}
          />
        </Card>
      </div>

      <aside className="quickstart-summary">
        <Card>
          <div className="row" style={{ justifyContent: "space-between" }}>
            <strong>{app.t("Ready to run", "运行就绪")}</strong>
            <Pill tone={readiness.every((item) => item.ready) ? "ok" : "warn"}>
              {readiness.filter((item) => item.ready).length}/{readiness.length}
            </Pill>
          </div>
          <div className="readiness-list">
            {readiness.map((item) => (
              <div className="readiness-item" key={item.label} data-ready={item.ready}>
                <span>{item.ready ? "✓" : "○"}</span>
                <span>
                  <strong>{item.label}</strong>
                  <small>{item.detail}</small>
                </span>
              </div>
            ))}
          </div>
          <div className="banner info" style={{ marginTop: 12 }}>
            <span>ⓘ</span>
            <span>{published
              ? app.t("This will publish the reviewed draft and create a new durable Session.", "将发布已审阅草稿并创建新的持久 Session。")
              : app.t("You will review the publication diff before the Agent becomes runnable.", "Agent 可运行前会先展示发布差异供你确认。")}</span>
          </div>
          <Button
            variant="primary"
            style={{ width: "100%", marginTop: 12, justifyContent: "center" }}
            disabled={!readiness.every((item) => item.ready) || runPending}
            onClick={() => onReviewRun(environmentId, task.trim())}
          >
            {runPending ? app.t("Starting…", "正在启动…") : app.t("Review, publish & run", "审阅、发布并运行")} ➤
          </Button>
        </Card>
      </aside>
    </div>
  );
}
