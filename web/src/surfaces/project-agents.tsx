// Project · Agents: the config-plane authoring list (GET /v1/config/agents).
// The console authors the rich agent object here directly — our own management
// API — rather than the SDK-facing /v1/agents registry. Publish (in the editor)
// compiles + installs a config so sessions run it. Row → the tabbed editor.

import { useQuery } from "@tanstack/react-query";
import { useNavigate, useParams } from "react-router";
import { DataGrid, type Column } from "../components/ui/DataGrid";
import { api } from "../lib/api/client";
import type { AgentConfig, AgentConfigItem, AgentConfigList } from "../lib/api/types";
import { useApp } from "../lib/app-state";
import { useListState } from "../lib/useListState";

function modelId(m: AgentConfig["model"]): string {
  return typeof m === "string" ? m : (m?.id ?? "");
}

export default function ProjectAgentsSurface() {
  const app = useApp();
  const nav = useNavigate();
  const { pid = "" } = useParams();
  const list = useListState("id");
  const agents = useQuery({
    queryKey: ["config-agents"],
    queryFn: () => api.get<AgentConfigList>("/v1/config/agents"),
    refetchInterval: 30_000,
  });
  const rows: AgentConfigItem[] = agents.data?.data ?? [];

  const columns: Column<AgentConfigItem>[] = [
    {
      key: "id",
      header: "Agent",
      sortValue: (a) => a.name || a.id,
      cell: (a) => <span className="mono">{a.name || a.id}</span>,
    },
    { key: "model", header: app.t("Model", "模型"), sortValue: (a) => modelId(a.model), cell: (a) => <span className="mono mut">{modelId(a.model) || "—"}</span> },
    { key: "tools", header: app.t("Tools", "工具"), sortValue: (a) => a.tools?.length ?? 0, cell: (a) => <span className="mut">{a.tools?.length ?? 0}</span> },
    { key: "plugins", header: app.t("Plugins", "插件"), sortValue: (a) => a.plugins?.length ?? 0, cell: (a) => <span className="mut">{a.plugins?.length ?? 0}</span> },
    {
      key: "status",
      header: app.t("Status", "状态"),
      sortValue: (a) => (a.published ? 1 : 0),
      cell: (a) =>
        a.published ? (
          <span className="pill ok">{app.t("published", "已发布")}</span>
        ) : (
          <span className="pill neutral">{app.t("draft", "草稿")}</span>
        ),
    },
  ];

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
      <DataGrid
        rows={rows}
        columns={columns}
        rowKey={(a) => a.id}
        state={list}
        loading={agents.isLoading}
        filter={(a, q) => (a.name || a.id).toLowerCase().includes(q.toLowerCase()) || modelId(a.model).toLowerCase().includes(q.toLowerCase())}
        onRowClick={(a) => nav(`/p/${pid}/agents/${a.id}`)}
        searchPlaceholder={app.t("Filter agents…", "过滤 agent…")}
        emptyTitle={app.t("No agents yet.", "还没有 Agent。")}
        emptyHint={app.t("Create one to author its model, tools, plugins and policy.", "新建一个来配置模型、工具、插件与策略。")}
      />
    </>
  );
}
