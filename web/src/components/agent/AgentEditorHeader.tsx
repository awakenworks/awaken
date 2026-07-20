import { Button, Pill } from "../ui";
import { useApp } from "../../lib/app-state";
import type { ReviewStatus } from "./useAgentDraftReview";

export default function AgentEditorHeader({
  id,
  isNew,
  dirty,
  status,
  rawOpen,
  canSave,
  validatePending,
  savePending,
  publishPending,
  onBack,
  onToggleRaw,
  onValidate,
  onSave,
  onPublish,
}: {
  id: string;
  isNew: boolean;
  dirty: boolean;
  status: ReviewStatus;
  rawOpen: boolean;
  canSave: boolean;
  validatePending: boolean;
  savePending: boolean;
  publishPending: boolean;
  onBack: () => void;
  onToggleRaw: () => void;
  onValidate: () => void;
  onSave: () => void;
  onPublish: () => void;
}) {
  const app = useApp();
  const busy = ["saving", "validating", "fixing"].includes(status);
  return (
    <div className="row" style={{ justifyContent: "space-between" }}>
      <span className="row">
        <Button variant="ghost" style={{ height: 26 }} onClick={onBack}>← {app.t("Agents", "Agents")}</Button>
        <span className="crumb-title mono">{isNew ? app.t("new agent", "新建 agent") : id}</span>
        {dirty && <Pill tone="warn">{app.t("unsaved", "未保存")}</Pill>}
        {status === "saving" && <Pill tone="neutral">{app.t("Saving draft…", "正在保存草稿…")}</Pill>}
        {status === "validating" && <Pill tone="agent">{app.t("Validating…", "正在校验…")}</Pill>}
        {status === "fixing" && <Pill tone="agent">✦ {app.t("Agent is fixing the draft…", "Agent 正在修复草稿…")}</Pill>}
        {status === "ready" && <Pill tone="ok">✓ {app.t("Validated", "校验通过")}</Pill>}
        {status === "needs_input" && <Pill tone="danger">{app.t("Needs input", "需要处理")}</Pill>}
      </span>
      <span className="row">
        <Button variant="ghost" aria-pressed={rawOpen} onClick={onToggleRaw}>
          {rawOpen ? app.t("Visual editor", "可视化编辑") : "{} JSON"}
        </Button>
        <Button variant="ghost" disabled={!canSave || validatePending} onClick={onValidate}>{app.t("Validate", "校验")}</Button>
        <Button disabled={!canSave || savePending} onClick={onSave}>{app.t("Save", "保存")}</Button>
        <Button
          variant="primary"
          disabled={!canSave || publishPending || busy}
          title={app.t("Saves, validates and auto-fixes before confirmation", "确认前自动保存、校验并修复")}
          onClick={onPublish}
        >
          {app.t("Publish", "发布")} ➤
        </Button>
      </span>
    </div>
  );
}
