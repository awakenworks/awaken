// Workspace · Overview: readiness and recent runtime activity for the active scope.
// session list (GET /v1/sessions via ws()). Observation only — the console
// operates the platform; end users interact through the SDK.

import { useQuery } from "@tanstack/react-query";
import { isManagedSessionActiveStatus } from "@awaken/managed-session-projection";
import { useNavigate, useParams } from "react-router";
import { api, ws } from "../lib/api/client";
import type { ListSessionsResponse } from "../lib/api/types";
import { useApp } from "../lib/app-state";
import { WORKSPACE_JOURNEY, hasSurface, navPath } from "../lib/navigation/paths";
import { useConfigCapabilities } from "../lib/useConfigCapabilities";
import { StatusPill } from "./sessions";
import { Button, Card } from "../components/ui";
import ReadinessPanel from "../components/app/ReadinessPanel";

export default function WorkspaceOverviewSurface() {
  const app = useApp();
  const nav = useNavigate();
  const { ws: wsId = "default" } = useParams();
  const capabilities = useConfigCapabilities();
  const managedRuntime = hasSurface(capabilities.data, "managed_runtime");
  const byokEnabled = capabilities.data?.models.byok_enabled === true;
  const journey = WORKSPACE_JOURNEY.filter((step) =>
    !step.destination.surface || hasSurface(capabilities.data, step.destination.surface));
  const sessions = useQuery({
    queryKey: ["sessions", wsId],
    queryFn: () => api.get<ListSessionsResponse>(ws("/v1/sessions")),
    refetchInterval: 15_000,
    enabled: managedRuntime,
  });
  const rows = sessions.data?.data ?? [];
  const active = rows.filter((s) => !s.archived_at);
  const running = active.filter((s) => isManagedSessionActiveStatus(s.status)).length;
  const recent = active.slice(0, 5);

  return (
    <div className="workspace-overview">
      <div className="page-intro workspace-overview__intro">
        <span><strong>{app.t("Choose where to continue", "选择下一步")}</strong><p className="mut">{app.t("Readiness checks below show what is already usable and what still needs setup.", "下方就绪检查会说明哪些能力已可用、哪些仍需配置。")}</p></span>
        <span className="row">
          <Button onClick={() => nav(`/w/${wsId}/models`)}>{app.t(byokEnabled ? "Connect provider" : "Browse models", byokEnabled ? "连接供应商" : "浏览模型")}</Button>
          <Button onClick={() => nav(`/w/${wsId}/agents/new`)}>{app.t("Create Agent", "创建 Agent")}</Button>
          {managedRuntime && <Button variant="primary" onClick={() => nav(`/w/${wsId}/sessions`)}>
            {app.t("Start real run", "开始真实运行")}
          </Button>}
        </span>
      </div>
      <nav className="execution-path" aria-label={app.t("Agent proof journey", "Agent 验证路径")}>
        {journey.map((step) => (
          <button key={step.number} type="button" onClick={() => nav(navPath(step.destination, wsId))}>
            <span>{step.number}</span>
            <strong>{app.t(step.label, step.labelZh)}</strong>
            <small>{app.t(step.detail, step.detailZh)}</small>
          </button>
        ))}
      </nav>
      <ReadinessPanel />
      {managedRuntime && sessions.error instanceof Error && (
        <div className="banner err">
          <span>{sessions.error.message}</span>
          <Button variant="ghost" onClick={() => void sessions.refetch()}>{app.t("Try again", "重试")}</Button>
        </div>
      )}
      {managedRuntime && <><div className="kpis">
        <button className="kpi" onClick={() => nav(`/w/${wsId}/sessions`)}>
          <span className="val">{sessions.data ? active.length : "—"}</span>
          <span className="label">{app.t("Active sessions", "活跃会话")}</span>
        </button>
        <button className="kpi" onClick={() => nav(`/w/${wsId}/sessions`)}>
          <span className="val" style={running ? { color: "var(--agent-ink)" } : undefined}>
            {sessions.data ? running : "—"}
          </span>
          <span className="label">{app.t("Running now", "正在运行")}</span>
        </button>
      </div>
      <Card style={{ padding: 0 }}>
        <div className="row session-ledger__header" style={{ padding: "13px 16px" }}>
          <span className="ledger-signal" aria-hidden="true" />
          <h2 style={{ margin: 0, fontSize: 14 }}>{app.t("Recent session evidence", "最近会话证据")}</h2>
          <span className="mut">{app.t("newest first", "最新在前")}</span>
        </div>
        <table className="table">
          <thead>
            <tr><th>{app.t("Session", "会话")}</th><th>{app.t("Title", "标题")}</th><th>{app.t("State", "状态")}</th><th><span className="sr-only">{app.t("Open", "打开")}</span></th></tr>
          </thead>
          <tbody>
            {recent.map((s) => (
              <tr key={s.id} data-click="true" onClick={() => nav(`/w/${wsId}/sessions/${s.id}`)}>
                <td className="mono">{s.id}</td>
                <td>{s.title || <span className="mut">(untitled)</span>}</td>
                <td>
                  <StatusPill session={s} />
                </td>
                <td style={{ textAlign: "right", color: "var(--fg3)" }}>▸</td>
              </tr>
            ))}
            {!sessions.error && recent.length === 0 && (
              <tr>
                <td className="mut" colSpan={4}>
                  {sessions.isLoading ? "…" : app.t("No sessions yet.", "还没有会话。")}
                </td>
              </tr>
            )}
          </tbody>
        </table>
      </Card></>}
    </div>
  );
}
