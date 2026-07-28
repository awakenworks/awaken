// Workspace · Overview: readiness and recent runtime activity for the active scope.
// session list (GET /v1/sessions via ws()). Observation only — the console
// operates the platform; end users interact through the SDK.

import { useQuery } from "@tanstack/react-query";
import { useNavigate, useParams } from "react-router";
import { api, ws } from "../lib/api/client";
import type { ListSessionsResponse } from "../lib/api/types";
import { useApp } from "../lib/app-state";
import { StatusPill } from "./sessions";
import { Button, Card } from "../components/ui";
import ReadinessPanel from "../components/app/ReadinessPanel";

export default function WorkspaceOverviewSurface() {
  const app = useApp();
  const nav = useNavigate();
  const { ws: wsId = "default" } = useParams();
  const sessions = useQuery({
    queryKey: ["sessions", wsId],
    queryFn: () => api.get<ListSessionsResponse>(ws("/v1/sessions")),
    refetchInterval: 15_000,
  });
  const rows = sessions.data?.data ?? [];
  const active = rows.filter((s) => !s.archived_at);
  const running = active.filter((s) => s.status === "running").length;
  const recent = active.slice(0, 5);

  return (
    <>
      <div className="page-intro">
        <span>
          <h1>{app.t("Workspace overview", "工作区概览")}</h1>
          <p className="mut">
            {app.t("Connect capabilities, build an Agent, then prove it with a real Session.", "连接能力、构建 Agent，并通过真实会话验证结果。")}
          </p>
        </span>
        <span className="row">
          <Button onClick={() => nav(`/w/${wsId}/models`)}>{app.t("Connect provider", "连接供应商")}</Button>
          <Button onClick={() => nav(`/w/${wsId}/agents/new`)}>{app.t("Create Agent", "创建 Agent")}</Button>
          <Button variant="primary" onClick={() => nav(`/w/${wsId}/sessions`)}>
            {app.t("Run a Session", "运行会话")}
          </Button>
        </span>
      </div>
      <ReadinessPanel />
      <div className="kpis">
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
        <div className="row" style={{ padding: "13px 16px" }}>
          <h2 style={{ margin: 0, fontSize: 14 }}>{app.t("Recent sessions", "最近会话")}</h2>
          <span className="mut">{app.t("newest first", "最新在前")}</span>
        </div>
        <table className="table">
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
            {recent.length === 0 && (
              <tr>
                <td className="mut">
                  {sessions.isLoading ? "…" : app.t("No sessions yet.", "还没有会话。")}
                </td>
              </tr>
            )}
          </tbody>
        </table>
      </Card>
    </>
  );
}
