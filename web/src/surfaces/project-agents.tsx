// Project · Agents: the config-plane authoring list (GET /v1/config/agents).
// The console authors the rich AgentConfig here directly — our own management
// API — rather than the SDK-facing /v1/agents registry. Publish (in the editor)
// compiles + installs a config so sessions run it. Row → the tabbed editor.

import { useQuery } from "@tanstack/react-query";
import { useNavigate, useParams } from "react-router";
import { api } from "../lib/api/client";
import type { AgentConfig, AgentConfigItem, AgentConfigList } from "../lib/api/types";
import { useApp } from "../lib/app-state";

function modelId(m: AgentConfig["model"]): string {
  return typeof m === "string" ? m : (m?.id ?? "");
}

export default function ProjectAgentsSurface() {
  const app = useApp();
  const nav = useNavigate();
  const { pid = "" } = useParams();
  const agents = useQuery({
    queryKey: ["config-agents"],
    queryFn: () => api.get<AgentConfigList>("/v1/config/agents"),
    refetchInterval: 30_000,
  });
  const rows: AgentConfigItem[] = agents.data?.data ?? [];

  return (
    <>
      <p className="mut" style={{ margin: 0 }}>
        {app.t(
          "Authored against the config plane (our own management API): basics, tools, plugins, policy, context. Publish compiles + installs a config so sessions run it.",
          "对接自有配置面(管理 API)作者:基础、工具、插件、策略、上下文。发布即编译并安装,session 随即以该配置运行。",
        )}
      </p>
      <div className="row" style={{ justifyContent: "flex-end" }}>
        <button className="btn primary" onClick={() => nav(`/p/${pid}/agents/new`)}>
          + {app.t("New agent", "新建 Agent")}
        </button>
      </div>
      {agents.error instanceof Error && <div className="err">{agents.error.message}</div>}
      <div className="card" style={{ padding: 0 }}>
        <table className="table">
          <thead>
            <tr>
              <th>Agent</th>
              <th>{app.t("Model", "模型")}</th>
              <th>{app.t("Tools", "工具")}</th>
              <th>{app.t("Plugins", "插件")}</th>
              <th>{app.t("Status", "状态")}</th>
            </tr>
          </thead>
          <tbody>
            {rows.map((a) => (
              <tr key={a.id} data-click="true" onClick={() => nav(`/p/${pid}/agents/${a.id}`)}>
                <td className="mono">{a.name || a.id}</td>
                <td className="mono mut">{modelId(a.model) || "—"}</td>
                <td className="mut">{a.tools?.length ?? 0}</td>
                <td className="mut">{a.plugins?.length ?? 0}</td>
                <td>
                  {a.published ? (
                    <span className="pill ok">{app.t("published", "已发布")}</span>
                  ) : (
                    <span className="pill neutral">{app.t("draft", "草稿")}</span>
                  )}
                </td>
              </tr>
            ))}
            {rows.length === 0 && (
              <tr>
                <td colSpan={5} className="mut">
                  {agents.isLoading ? "…" : app.t("No agents yet.", "还没有 Agent。")}
                </td>
              </tr>
            )}
          </tbody>
        </table>
      </div>
    </>
  );
}
