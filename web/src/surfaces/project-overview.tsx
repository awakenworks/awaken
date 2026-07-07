// Project · Overview: the container's live pulse, fed by the project-scoped
// session list (GET /projects/{pid}/v1/sessions). Observation only — the
// console operates the platform; end users interact through the SDK.

import { useQuery } from "@tanstack/react-query";
import { useNavigate, useParams } from "react-router";
import { api } from "../lib/api/client";
import type { ListSessionsResponse } from "../lib/api/types";
import { useApp } from "../lib/app-state";
import { StatusPill } from "./sessions";

export default function ProjectOverviewSurface() {
  const app = useApp();
  const nav = useNavigate();
  const { pid = "" } = useParams();
  const sessions = useQuery({
    queryKey: ["sessions", pid],
    queryFn: () => api.get<ListSessionsResponse>(`/projects/${pid}/v1/sessions`),
    refetchInterval: 15_000,
  });
  const rows = sessions.data?.data ?? [];
  const active = rows.filter((s) => !s.archived_at);
  const running = active.filter((s) => s.status === "running").length;
  const recent = active.slice(0, 5);

  return (
    <>
      <p className="mut" style={{ margin: 0 }}>
        {pid} · {app.t("this project's runtime right now.", "本项目当前的运行面。")}
      </p>
      <div className="kpis">
        <button className="kpi" onClick={() => nav(`/p/${pid}/sessions`)}>
          <span className="val">{sessions.data ? active.length : "—"}</span>
          <span className="label">{app.t("Active sessions", "活跃会话")}</span>
        </button>
        <button className="kpi" onClick={() => nav(`/p/${pid}/sessions`)}>
          <span className="val" style={running ? { color: "var(--agent-ink)" } : undefined}>
            {sessions.data ? running : "—"}
          </span>
          <span className="label">{app.t("Running now", "正在运行")}</span>
        </button>
        <button className="kpi" onClick={() => nav(`/p/${pid}/vaults`)}>
          <span className="val">→</span>
          <span className="label">Vaults</span>
        </button>
        <button className="kpi" onClick={() => nav(`/p/${pid}/agents`)}>
          <span className="val">→</span>
          <span className="label">{app.t("Agent MCP bindings", "Agent MCP 绑定")}</span>
        </button>
      </div>
      <div className="card" style={{ padding: 0 }}>
        <div className="row" style={{ padding: "13px 16px" }}>
          <h2 style={{ margin: 0, fontSize: 14 }}>{app.t("Recent sessions", "最近会话")}</h2>
          <span className="mut">{app.t("newest first", "最新在前")}</span>
        </div>
        <table className="table">
          <tbody>
            {recent.map((s) => (
              <tr key={s.id} data-click="true" onClick={() => nav(`/p/${pid}/sessions/${s.id}`)}>
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
      </div>
    </>
  );
}
