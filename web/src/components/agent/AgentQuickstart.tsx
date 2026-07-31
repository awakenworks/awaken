import { useQuery } from "@tanstack/react-query";
import { useState } from "react";
import type { AgentConfig, Environment, Page, RuntimeCap } from "../../lib/api/types";
import { api, ws } from "../../lib/api/client";
import { useApp } from "../../lib/app-state";
import AgentModelSelectionEditor from "./AgentModelSelectionEditor";
import { Button, Card, Pill, TextAreaField, TextField } from "../ui";

interface StarterTemplate {
  id: string;
  title: string;
  titleZh: string;
  description: string;
  descriptionZh: string;
  name: string;
  system: string;
  maxSteps: number;
  suggestedTools: string[];
  suggestedPlugins?: string[];
}

const STARTERS: StarterTemplate[] = [
  {
    id: "blank",
    title: "Blank",
    titleZh: "空白",
    description: "A minimal agent you shape from scratch.",
    descriptionZh: "从最小配置开始，自行塑造能力。",
    name: "New Agent",
    system: "You are a helpful agent. Follow the user's goal carefully and explain important decisions.",
    maxSteps: 8,
    suggestedTools: [],
  },
  {
    id: "coding",
    title: "Coding",
    titleZh: "代码助手",
    description: "Inspect, change, and verify a codebase.",
    descriptionZh: "检查、修改并验证代码仓。",
    name: "Coding Agent",
    system: "You are a careful coding agent. Inspect the repository, make focused changes, and verify behavior before reporting completion.",
    maxSteps: 16,
    suggestedTools: ["bash", "read", "write", "glob", "grep"],
  },
  {
    id: "research",
    title: "Research",
    titleZh: "研究助手",
    description: "Gather evidence and produce a sourced synthesis.",
    descriptionZh: "收集证据并形成有来源的综合结论。",
    name: "Research Agent",
    system: "You are a research agent. Gather evidence, separate observation from inference, and produce a concise sourced synthesis.",
    maxSteps: 12,
    suggestedTools: ["web_search"],
    suggestedPlugins: ["web_search"],
  },
];

export default function AgentQuickstart({
  config,
  readyModels,
  allModels,
  runtimes,
  availableTools,
  availablePlugins,
  canRun,
  idEditable,
  published,
  runPending,
  onPatch,
  onManageModels,
  onReviewRun,
}: {
  config: AgentConfig;
  readyModels: string[];
  allModels: string[];
  runtimes: RuntimeCap[];
  availableTools: string[];
  availablePlugins: string[];
  canRun: boolean;
  idEditable: boolean;
  published: boolean;
  runPending: boolean;
  onPatch: (patch: Partial<AgentConfig>) => void;
  onManageModels: () => void;
  onReviewRun: (environmentId: string | undefined, task: string) => void;
}) {
  const app = useApp();
  const [selectedTemplate, setSelectedTemplate] = useState("blank");
  const [environmentId, setEnvironmentId] = useState("");
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
    const plugins = (template.suggestedPlugins ?? []).filter((id) => availablePlugins.includes(id));
    onPatch({
      name: template.name,
      description: app.t(template.description, template.descriptionZh),
      system: template.system,
      max_steps: template.maxSteps,
      tools: Array.from(new Set([...config.tools, ...tools])),
      plugins: Array.from(new Set([...config.plugins, ...plugins])),
    });
  };

  const readiness = [
    {
      ready: config.id.trim().length > 0,
      label: app.t("Agent id", "Agent id"),
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
            </button>
          ))}
        </div>
        <Card className="quickstart-step">
          <h2 className="section-title">{app.t("Agent identity", "Agent 标识")}</h2>
          <TextField
            label={app.t("Agent id", "Agent id")}
            mono
            placeholder="coding-agent"
            value={config.id}
            disabled={!idEditable}
            onChange={(event) => onPatch({ id: event.target.value })}
          />
        </Card>

        <Card className="quickstart-step">
          <h2 className="section-title">{app.t("2 · Choose a runnable model", "2 · 选择可运行模型")}</h2>
          <AgentModelSelectionEditor
            model={config.model}
            readyModels={readyModels}
            allModels={allModels}
            runtimes={runtimes}
            onChange={(model) => onPatch({ model })}
            onManage={onManageModels}
          />
        </Card>

        <Card className="quickstart-step">
          <h2 className="section-title">{app.t("3 · Choose where the first run executes", "3 · 选择首次运行环境")}</h2>
          <label className="field">
            <span>{app.t("Environment for this run", "本次运行的 Environment")}</span>
            <select
              className="input mono"
              aria-label={app.t("Environment for this run", "本次运行的 Environment")}
              value={environmentId}
              onChange={(event) => setEnvironmentId(event.target.value)}
            >
              <option value="">{app.t("Default runtime", "默认运行环境")}</option>
              {(environments.data?.data ?? []).map((environment) => (
                <option key={environment.id} value={environment.id}>
                  {environment.name} · {environment.id}
                </option>
              ))}
            </select>
          </label>
          <span className="mut">{app.t(
            "Environment is run-scoped. It is not silently stored as an Agent property.",
            "Environment 属于本次运行，不会被悄悄保存成 Agent 固有属性。",
          )}</span>
          {environments.error instanceof Error && <div className="err">{environments.error.message}</div>}
        </Card>

        <Card className="quickstart-step">
          <h2 className="section-title">{app.t("4 · Define the first real task", "4 · 定义首次真实任务")}</h2>
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
            onClick={() => onReviewRun(environmentId || undefined, task.trim())}
          >
            {runPending ? app.t("Starting…", "正在启动…") : app.t("Review & run", "审阅并运行")} ➤
          </Button>
        </Card>
      </aside>
    </div>
  );
}
