import { useNavigate, useParams } from "react-router";
import { useApp } from "../lib/app-state";
import { sessionRegistry } from "./sessions";

export default function ProjectOverviewSurface() {
  const app = useApp();
  const nav = useNavigate();
  const { pid = "" } = useParams();
  const known = sessionRegistry(pid);
  return (
    <>
      <p className="mut" style={{ margin: 0 }}>
        {pid} · {app.t("this project's runtime right now.", "本项目当前的运行面。")}
      </p>
      <div className="kpis">
        <button className="kpi" onClick={() => nav(`/p/${pid}/sessions`)}>
          <span className="val">{known.length}</span>
          <span className="label">{app.t("Known sessions (browser)", "已知会话(本浏览器)")}</span>
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
      <div className="banner gate">
        <span>◌</span>
        <span>
          {app.t(
            "Live pulse (active sessions, recent dispatches) needs the session-list and runs-summary faces — roadmap §7.",
            "实时脉搏(活跃会话、最近 dispatch)需要会话列表与 runs-summary 端点——路线 §7。",
          )}
        </span>
      </div>
    </>
  );
}
