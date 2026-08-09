// Workspace · Agents: the config-plane authoring list (GET /v1/config/agents).
// The console authors the rich agent object here directly — our own management
// API — rather than the SDK-facing /v1/agents registry. Publish (in the editor)
// compiles + installs a config so sessions run it. Row → the tabbed editor.

import { useQuery } from "@tanstack/react-query";
import { useNavigate, useParams, useSearchParams } from "react-router";
import { DataGrid, type Column } from "../components/ui/DataGrid";
import { Button, Pill, Segmented } from "../components/ui";
import AgentCollaborationsOverview from "../components/agent/AgentCollaborationsOverview";
import { api, ws } from "../lib/api/client";
import type { AgentConfig, AgentConfigItem, AgentConfigList } from "../lib/api/types";
import { useApp } from "../lib/app-state";
import { useListState } from "../lib/useListState";
import { authoredAgents, visibleAgents } from "../lib/visible-agents";

function modelId(m: AgentConfig["model"]): string {
  if (typeof m === "string") return m;
  if ("id" in m) return m.id;
  if (m.mode === "backend_default") return `${m.backend_ref} (default)`;
  if (m.mode === "backend_exact") return `${m.backend_ref}/${m.model_ref}`;
  if (m.mode === "profile") return `profile:${m.profile_id}`;
  if (m.mode === "pinned") return m.model_ref;
  return "auto";
}

export default function AgentsSurface() {
  const app = useApp();
  const nav = useNavigate();
  const { ws: wsId = "default" } = useParams();
  const [searchParams, setSearchParams] = useSearchParams();
  const view = searchParams.get("view") === "collaborations" ? "collaborations" : "all";
  const list = useListState("id");
  const agents = useQuery({
    queryKey: ["config-agents", wsId],
    queryFn: () => api.get<AgentConfigList>(ws("/v1/config/agents")),
    refetchInterval: 30_000,
  });
  const allAuthored: AgentConfigItem[] = authoredAgents(agents.data?.data);
  const rows: AgentConfigItem[] = visibleAgents(agents.data?.data);

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
          <Pill tone="ok">{app.t("published", "已发布")}</Pill>
        ) : (
          <Pill tone="neutral">{app.t("draft", "草稿")}</Pill>
        ),
    },
  ];

  return (
    <>
      <div className="row" style={{ justifyContent: "flex-end", gap: 8 }}>
        {/* The Admin Assistant is an authoring aid, reached from here (not the rail):
            describe an agent in plain English and it drafts one for you. */}
        <Button variant="ghost" onClick={() => nav(`/w/${wsId}/assistant`)}>
          ✦ {app.t("Ask Assistant", "询问助手")}
        </Button>
        <Button variant="primary" onClick={() => nav(`/w/${wsId}/agents/new`)}>
          + {app.t("New agent", "新建 Agent")}
        </Button>
      </div>
      <Segmented
        className="agent-list-views"
        options={[
          { value: "all", label: app.t("All Agents", "全部 Agent") },
          { value: "collaborations", label: app.t("Collaborations", "协作关系") },
        ]}
        value={view}
        onChange={(next) => setSearchParams(next === "collaborations" ? { view: next } : {}, { replace: true })}
      />
      {agents.error instanceof Error && <div className="err">{agents.error.message}</div>}
      {view === "all" ? <DataGrid
        rows={rows}
        columns={columns}
        rowKey={(a) => a.id}
        state={list}
        loading={agents.isLoading}
        filter={(a, q) => (a.name || a.id).toLowerCase().includes(q.toLowerCase()) || modelId(a.model).toLowerCase().includes(q.toLowerCase())}
        onRowClick={(a) => nav(`/w/${wsId}/agents/${a.id}`)}
        searchPlaceholder={app.t("Filter agents…", "过滤 agent…")}
        emptyTitle={app.t("No agents yet.", "还没有 Agent。")}
        emptyHint={app.t("Create an Agent, choose a model, and complete its first real run in Quickstart.", "新建 Agent、选择模型，并在“快速开始”中完成首次真实运行。")}
      /> : <AgentCollaborationsOverview agents={allAuthored} />}
    </>
  );
}
