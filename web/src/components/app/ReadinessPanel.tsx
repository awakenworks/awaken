import { useNavigate } from "react-router";
import { useApp } from "../../lib/app-state";
import { useWorkspaceReadiness } from "../../lib/readiness";
import { Card, Pill, Skeleton } from "../ui";

export default function ReadinessPanel({ compact = false }: { compact?: boolean }) {
  const app = useApp();
  const nav = useNavigate();
  const readiness = useWorkspaceReadiness();
  return (
    <Card className="readiness-panel">
      <div className="readiness-head">
        <span>
          <h2>{app.t("Workspace readiness", "工作区就绪状态")}</h2>
          <p className="hint">
            {app.t(
              "Check whether a model, a published Agent, and an execution Environment are ready before starting a Session.",
              "开始 Session 前，检查模型、已发布 Agent 和执行 Environment 是否已经就绪。",
            )}
          </p>
        </span>
        <Pill tone={readiness.ready ? "ok" : "warn"}>
          {readiness.loading
            ? app.t("checking", "检查中")
            : readiness.ready
              ? app.t("ready", "已就绪")
              : app.t("action required", "需要处理")}
        </Pill>
      </div>
      {readiness.loading ? (
        <Skeleton height={compact ? 44 : 72} />
      ) : (
        <div className="readiness-items">
          {readiness.items.map((item) => (
            <button key={item.id} className="readiness-item" onClick={() => nav(item.href)}>
              <span className={`readiness-icon ${item.status}`}>
                {item.status === "ready" ? "✓" : item.status === "action" ? "→" : "!"}
              </span>
              <span>
                <strong>{item.label}</strong>
                {!compact && <small>{item.detail}</small>}
              </span>
              <span className="readiness-action">
                {item.status === "ready" ? app.t("Review", "查看") : app.t("Set up", "去配置")}
              </span>
            </button>
          ))}
        </div>
      )}
    </Card>
  );
}
