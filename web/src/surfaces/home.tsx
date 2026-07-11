// Global Home: the cross-workspace cockpit. KPIs read the Managed Agents
// `GET /v1/sessions` face (default scope) with client-side derivation only —
// Anthropic's list is not server-filtered, so "running" etc. are computed here
// from `session.status`. This console is the platform's operations surface;
// end-user interaction (chat, approvals) lives in the customer's own apps.

import { useQuery } from "@tanstack/react-query";
import { useNavigate } from "react-router";
import { api } from "../lib/api/client";
import type { ListSessionsResponse } from "../lib/api/types";
import { useApp } from "../lib/app-state";
import { Card } from "../components/ui";

export default function HomeSurface() {
  const app = useApp();
  const nav = useNavigate();
  const sessions = useQuery({
    queryKey: ["sessions", "all"],
    queryFn: () => api.get<ListSessionsResponse>("/v1/sessions"),
    refetchInterval: 20_000,
  });
  const all = sessions.data?.data ?? [];
  const active = all.filter((s) => !s.archived_at);
  const running = active.filter((s) => s.status === "running").length;

  return (
    <>
      <p className="mut" style={{ margin: 0 }}>
        {app.t("Everything you oversee, across every workspace.", "你所监督的一切,跨越所有工作区。")}
      </p>
      <div className="kpis">
        <div className="kpi" style={{ cursor: "default" }}>
          <span className="val">{app.workspaces.length}</span>
          <span className="label">{app.t("Workspaces", "工作区")}</span>
        </div>
        <div className="kpi" style={{ cursor: "default" }}>
          <span className="val">{sessions.data ? active.length : "—"}</span>
          <span className="label">{app.t("Active sessions (this scope)", "活跃会话(当前作用域)")}</span>
        </div>
        <div className="kpi" style={{ cursor: "default" }}>
          <span className="val" style={running ? { color: "var(--agent-ink)" } : undefined}>
            {sessions.data ? running : "—"}
          </span>
          <span className="label">{app.t("Running now", "正在运行")}</span>
        </div>
      </div>
      <Card style={{ padding: 0 }}>
        <div className="row" style={{ padding: "13px 16px" }}>
          <h2 style={{ margin: 0, fontSize: 14 }}>{app.t("Workspaces", "工作区")}</h2>
          <span className="mut">{app.t("click one to enter it", "点击进入")}</span>
        </div>
        <table className="table">
          <tbody>
            {app.workspaces.map((w) => (
              <tr
                key={w.id}
                data-click="true"
                onClick={() => {
                  app.setWorkspaceId(w.id);
                  nav(`/w/${w.id}/overview`);
                }}
              >
                <td className="mono">{w.id}</td>
                <td>{w.display_name}</td>
                <td style={{ textAlign: "right", color: "var(--fg3)" }}>▸</td>
              </tr>
            ))}
          </tbody>
        </table>
      </Card>
    </>
  );
}
