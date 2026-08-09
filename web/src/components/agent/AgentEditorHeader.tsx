import { Button, Pill } from "../ui";
import { useApp } from "../../lib/app-state";
import type { ReviewStatus } from "./useAgentDraftReview";

export default function AgentEditorHeader({
  id,
  isNew,
  dirty,
  status,
  canSave,
  validatePending,
  savePending,
  publishPending,
  onBack,
  onValidate,
  onSave,
  onPublish,
}: {
  id: string;
  isNew: boolean;
  dirty: boolean;
  status: ReviewStatus;
  canSave: boolean;
  validatePending: boolean;
  savePending: boolean;
  publishPending: boolean;
  onBack: () => void;
  onValidate: () => void;
  onSave: () => void;
  onPublish: () => void;
}) {
  const app = useApp();
  const busy = ["saving", "validating", "fixing"].includes(status);
  return (
    <div className="row" style={{ justifyContent: "space-between" }}>
      <span className="row">
        <Button variant="ghost" style={{ height: 26 }} onClick={onBack}>← {app.t("Agents", "Agent")}</Button>
        <span className="crumb-title mono">{isNew ? app.t("New Agent", "新建 Agent") : id}</span>
        {dirty && <Pill tone="warn">{app.t("unsaved", "未保存")}</Pill>}
        {status === "saving" && <Pill tone="neutral">{app.t("Saving draft…", "正在保存草稿…")}</Pill>}
        {status === "validating" && <Pill tone="agent">{app.t("Validating…", "正在校验…")}</Pill>}
        {status === "fixing" && <Pill tone="agent">✦ {app.t("Agent is fixing the draft…", "Agent 正在修复草稿…")}</Pill>}
        {status === "ready" && <Pill tone="ok">✓ {app.t("Draft checked", "草稿检查通过")}</Pill>}
        {status === "needs_input" && <Pill tone="danger">{app.t("Needs input", "需要处理")}</Pill>}
      </span>
      <span className="row">
        <Button variant="ghost" disabled={!canSave || validatePending} onClick={onValidate}>{app.t("Check draft", "检查草稿")}</Button>
        <Button disabled={!canSave || savePending} onClick={onSave} title={app.t("Optional: keep this draft for later", "可选：保存草稿供稍后继续")}>{app.t("Save draft", "保存草稿")}</Button>
        <Button
          variant="primary"
          disabled={!canSave || publishPending || busy}
          title={app.t("Saves and checks the draft, then shows the exact changes before publishing", "保存并检查草稿，然后在发布前展示确切差异")}
          onClick={onPublish}
        >
          {app.t("Review & publish", "审阅并发布")} ➤
        </Button>
      </span>
    </div>
  );
}
