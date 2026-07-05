import { useNavigate } from "react-router";
import { useApp } from "../lib/app-state";
import { sessionRegistry } from "./sessions";

export default function HomeSurface() {
  const app = useApp();
  const nav = useNavigate();
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
        <button className="kpi" onClick={() => nav("/inbox")}>
          <span className="val">—</span>
          <span className="label">{app.t("Needs you (gap §7.2)", "待处理(缺口 §7.2)")}</span>
        </button>
        <button className="kpi" onClick={() => nav("/dashboard")}>
          <span className="val">—</span>
          <span className="label">{app.t("Active sessions (gap §7.1)", "活跃会话(缺口 §7.1)")}</span>
        </button>
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
                  {sessionRegistry(p.id).length} {app.t("known sessions", "已知会话")}
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
