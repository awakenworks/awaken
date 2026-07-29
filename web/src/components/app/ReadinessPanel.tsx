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
          <h2>{app.t("Ready to run", "运行就绪")}</h2>
          <p className="hint">
            {app.t(
              "Live projection of the existing Workspace configuration; no settings are copied here.",
              "现有工作区配置的实时投影；此处不复制任何设置。",
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
                {item.status === "ready" ? app.t("View", "查看") : app.t("Fix", "修复")}
              </span>
            </button>
          ))}
        </div>
      )}
    </Card>
  );
}
