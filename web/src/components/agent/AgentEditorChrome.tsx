import type { AgentConfig, ValidationIssue } from "../../lib/api/types";
import { labelForPath } from "../../lib/config-diff";
import { CONSERVATIVE_DELEGATION_LIMITS } from "../../lib/agent-collaboration";
import { useApp } from "../../lib/app-state";
import { Button } from "../ui";
import type { AuthorStage } from "./agent-editor-navigation";

export const BLANK_AGENT_CONFIG: AgentConfig = {
  id: "",
  name: "",
  model: { mode: "auto" },
  system: "You are a helpful coding agent.",
  metadata: {},
  tools: [],
  mcp_servers: [],
  skills: [],
  multiagent: { type: "coordinator", agents: [{ type: "self" }] },
  delegation_limits: CONSERVATIVE_DELEGATION_LIMITS,
  max_steps: 8,
  plugins: [],
  plugin_config: {},
  context_policy: { kind: "keep_all" },
};

const STAGES: Array<{
  key: AuthorStage;
  label: string;
  zh: string;
  description: string;
  descriptionZh: string;
}> = [
  { key: "quickstart", label: "Quickstart", zh: "快速开始", description: "Template to first real run", descriptionZh: "从模板到首次真实运行" },
  { key: "build", label: "Build", zh: "构建", description: "Prompt, capabilities and knowledge", descriptionZh: "提示词、能力与知识" },
  { key: "advanced", label: "Advanced", zh: "高级", description: "Orchestration, plugins and source", descriptionZh: "编排、Plugin 与原始配置" },
];

export function AgentEditorStageNavigation({
  stage,
  changed,
  onChange,
  canTry,
  onTry,
}: {
  stage: AuthorStage;
  changed: (stage: AuthorStage) => boolean;
  onChange: (stage: AuthorStage) => void;
  canTry: boolean;
  onTry: () => void;
}) {
  const app = useApp();
  return (
    <div className="author-stage-nav" role="tablist" aria-label={app.t("Authoring stages", "创作阶段")}>
      {STAGES.map((item, index) => (
        <button
          key={item.key}
          className="author-stage-button"
          role="tab"
          aria-label={app.t(item.label, item.zh)}
          aria-selected={stage === item.key}
          data-active={stage === item.key}
          onClick={() => onChange(item.key)}
        >
          <span className="author-stage-index">{index + 1}</span>
          <span>
            <strong>{app.t(item.label, item.zh)}</strong>
            <small>{app.t(item.description, item.descriptionZh)}</small>
          </span>
          {changed(item.key) && <span className="agent-change-dot">✦</span>}
        </button>
      ))}
      {canTry && <Button variant="ghost" onClick={onTry}>▷ {app.t("Try draft", "试运行草稿")}</Button>}
    </div>
  );
}

export function AgentValidationIssues({
  issues,
  onOpen,
}: {
  issues: ValidationIssue[];
  onOpen: (path: string) => void;
}) {
  const app = useApp();
  if (issues.length === 0) return null;
  return (
    <div className="banner warn issue-banner">
      {issues.map((issue, index) => (
        <div className="row" key={`${issue.path}-${index}`} style={{ justifyContent: "space-between" }}>
          <span><strong>{labelForPath(issue.path) || app.t("Config", "配置")}</strong>{" — "}{issue.message}</span>
          <Button variant="ghost" onClick={() => onOpen(issue.path)}>{app.t("Open field →", "打开对应字段 →")}</Button>
        </div>
      ))}
    </div>
  );
}

export function AttachedAgentContext({ parentId, onBack }: { parentId: string; onBack: () => void }) {
  const app = useApp();
  if (!parentId) return null;
  return (
    <div className="banner info attached-agent-context">
      <span>↳</span>
      <span>
        <strong>{app.t(`Specialist for ${parentId}`, `${parentId} 的专属辅助 Agent`)}</strong><br />
        {app.t(
          "This Agent belongs to the primary Agent above. Publish it first, then the parent roster will be updated for review.",
          "此 Agent 归属于上方主 Agent。请先发布它，随后系统会更新主 Agent 清单供你审阅。",
        )}
      </span>
      <Button variant="ghost" style={{ marginLeft: "auto" }} onClick={onBack}>
        {app.t("Back to primary Agent", "返回主 Agent")}
      </Button>
    </div>
  );
}
