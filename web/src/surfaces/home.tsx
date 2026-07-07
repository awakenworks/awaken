// Global Home: the cross-project cockpit. KPIs aggregate the bare
// GET /v1/sessions face (the workspace view); "Needs you" is the server's
// derived requires_action filter. This console is the platform's operations
// surface — end-user interaction (chat, approvals) lives in the customer's
// own apps over the SDK, so the numbers here are observational.

import { useQuery } from "@tanstack/react-query";
import { useNavigate } from "react-router";
import { api } from "../lib/api/client";
import type { ListSessionsResponse } from "../lib/api/types";
import { useApp } from "../lib/app-state";

export default function HomeSurface() {
  const app = useApp();
  const nav = useNavigate();
  const sessions = useQuery({
    queryKey: ["sessions", "all"],
    queryFn: () => api.get<ListSessionsResponse>("/v1/sessions"),
    refetchInterval: 30_000,
  });
  const needsYou = useQuery({
    queryKey: ["sessions", "needs-you"],
    queryFn: () => api.get<ListSessionsResponse>("/v1/sessions?status=requires_action"),
    refetchInterval: 15_000,
  });
  const all = sessions.data?.data ?? [];
  const active = all.filter((s) => !s.archived_at);
  const byProject = new Map<string, number>();
  for (const s of active) {
    if (s.project_id) byProject.set(s.project_id, (byProject.get(s.project_id) ?? 0) + 1);
  }
  const needsYouCount = needsYou.data?.data.length;

  return (
    <>
      <p className="mut" style={{ margin: 0 }}>
        {app.t("Everything you oversee, across every project.", "你所监督的一切,跨越所有项目。")}
      </p>
      <div className="kpis">
        <button className="kpi" onClick={() => app.setScope("workspace")}>
          <span className="val">{app.projects.length}</span>
          <span className="label">Projects</span>
        </button>
        <div className="kpi" style={{ cursor: "default" }}>
          <span className="val" style={needsYouCount ? { color: "var(--accent-ink)" } : undefined}>
            {needsYouCount ?? "—"}
          </span>
          <span className="label">{app.t("Awaiting client action", "等待客户端动作")}</span>
        </div>
        <div className="kpi" style={{ cursor: "default" }}>
          <span className="val">{sessions.data ? active.length : "—"}</span>
          <span className="label">{app.t("Active sessions", "活跃会话")}</span>
        </div>
      </div>
      <div className="card" style={{ padding: 0 }}>
        <div className="row" style={{ padding: "13px 16px" }}>
          <h2 style={{ margin: 0, fontSize: 14 }}>Projects</h2>
          <span className="mut">{app.t("click one to enter it", "点击进入项目")}</span>
        </div>
        <table className="table">
          <tbody>
            {app.projects.map((p) => (
              <tr
                key={p.id}
                data-click="true"
                onClick={() => {
                  app.setProjectId(p.id);
                  app.setScope("project");
                  nav(`/p/${p.id}/overview`);
                }}
              >
                <td className="mono">{p.id}</td>
                <td>{p.display_name}</td>
                <td className="mut">
                  {byProject.get(p.id) ?? 0} {app.t("active sessions", "活跃会话")}
                </td>
                <td style={{ textAlign: "right", color: "var(--fg3)" }}>▸</td>
              </tr>
            ))}
            {app.projects.length === 0 && (
              <tr>
                <td className="mut">
                  {app.t("No projects yet — author one via PUT /v1/config/projects/:id or in project settings.", "还没有项目。")}
                </td>
              </tr>
            )}
          </tbody>
        </table>
      </div>
    </>
  );
}
