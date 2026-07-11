// Project · Overview: the container's live pulse, fed by the workspace-scoped
// session list (GET /v1/sessions via ws()). Observation only — the console
// operates the platform; end users interact through the SDK.

import { useQuery } from "@tanstack/react-query";
import { useNavigate, useParams } from "react-router";
import { api, ws } from "../lib/api/client";
import type { ListSessionsResponse } from "../lib/api/types";
import { useApp } from "../lib/app-state";
import { StatusPill } from "./sessions";
import { Card } from "../components/ui";

export default function ProjectOverviewSurface() {
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
      <p className="mut" style={{ margin: 0 }}>
        {wsId} · {app.t("this workspace's runtime right now.", "本工作区当前的运行面。")}
      </p>
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
        <button className="kpi" onClick={() => nav(`/w/${wsId}/vaults`)}>
          <span className="val">→</span>
          <span className="label">Vaults</span>
        </button>
        <button className="kpi" onClick={() => nav(`/w/${wsId}/agents`)}>
          <span className="val">→</span>
          <span className="label">{app.t("Agent MCP bindings", "Agent MCP 绑定")}</span>
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
