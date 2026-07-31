import { useQuery } from "@tanstack/react-query";
import { Link, useNavigate, useParams } from "react-router";
import { DataGrid, type Column } from "../components/ui/DataGrid";
import { Button, Pill } from "../components/ui";
import { api, ws } from "../lib/api/client";
import type { AgentConfigList } from "../lib/api/types";
import { useApp } from "../lib/app-state";
import { useListState } from "../lib/useListState";

interface McpInventoryRow {
  key: string;
  name: string;
  url: string;
  promptsAsSkills: boolean;
  credential?: string;
  agents: string[];
}

function objectOf(value: unknown): Record<string, unknown> {
  return value && typeof value === "object" && !Array.isArray(value)
    ? value as Record<string, unknown>
    : {};
}

function credentialLabel(value: unknown): string | undefined {
  const credential = objectOf(value);
  if (typeof credential.id !== "string") return undefined;
  return typeof credential.revision === "number"
    ? `${credential.id}@${credential.revision}`
    : credential.id;
}

export default function McpOverviewSurface() {
  const app = useApp();
  const nav = useNavigate();
  const { ws: wsId = "default" } = useParams();
  const list = useListState("name");
  const agents = useQuery({
    queryKey: ["config-agents", wsId],
    queryFn: () => api.get<AgentConfigList>(ws("/v1/config/agents")),
    refetchInterval: 30_000,
  });
  const inventory = new Map<string, McpInventoryRow>();
  for (const agent of agents.data?.data ?? []) {
    for (const raw of agent.mcp_servers ?? []) {
      const server = objectOf(raw);
      const name = typeof server.name === "string" ? server.name : "";
      const url = typeof server.url === "string" ? server.url : "";
      if (!name || !url) continue;
      const key = `${name}\u0000${url}`;
      const existing = inventory.get(key);
      if (existing) {
        if (!existing.agents.includes(agent.id)) existing.agents.push(agent.id);
      } else {
        inventory.set(key, {
          key,
          name,
          url,
          promptsAsSkills: server.prompts_as_skills === true,
          credential: credentialLabel(server.credential),
          agents: [agent.id],
        });
      }
    }
  }
  const rows = [...inventory.values()];
  const columns: Column<McpInventoryRow>[] = [
    {
      key: "name",
      header: app.t("MCP server", "MCP 服务器"),
      sortValue: (row) => row.name,
      cell: (row) => (
        <span style={{ display: "flex", flexDirection: "column", gap: 3 }}>
          <strong>{row.name}</strong>
          <code className="mut">{row.url}</code>
        </span>
      ),
    },
    {
      key: "skills",
      header: app.t("Prompts", "Prompts"),
      sortValue: (row) => row.promptsAsSkills ? 1 : 0,
      cell: (row) => row.promptsAsSkills
        ? <Pill tone="agent">{app.t("as remote Skills", "作为远程 Skills")}</Pill>
        : <Pill tone="neutral">{app.t("ordinary prompts", "普通 Prompts")}</Pill>,
    },
    {
      key: "credential",
      header: app.t("Credential reference", "凭据引用"),
      sortValue: (row) => row.credential ?? "",
      cell: (row) => <code className="mut">{row.credential ?? app.t("unauthenticated", "无认证")}</code>,
    },
    {
      key: "agents",
      header: app.t("Used by Agents", "使用此连接的 Agents"),
      sortValue: (row) => row.agents.length,
      cell: (row) => (
        <span className="row" style={{ flexWrap: "wrap" }}>
          {row.agents.map((agentId) => (
            <Link key={agentId} to={`/w/${wsId}/agents/${agentId}?stage=build&section=integrations`}>
              <Pill tone="info">{agentId}</Pill>
            </Link>
          ))}
        </span>
      ),
    },
    {
      key: "status",
      header: app.t("Status", "状态"),
      cell: () => <Pill tone="neutral">{app.t("configured", "已配置")}</Pill>,
    },
  ];

  return (
    <>
      <div className="row" style={{ justifyContent: "space-between", alignItems: "flex-start" }}>
        <p className="mut" style={{ margin: 0, maxWidth: 780 }}>
          {app.t(
            "Read-only inventory derived from Agent drafts. MCP endpoints are authored and published inside each Agent; this page never creates a second connection registry.",
            "这是从 Agent 草稿派生的只读清单。MCP endpoint 在各 Agent 内配置并发布；本页不会创建第二套连接目录。",
          )}
        </p>
        <Button variant="primary" onClick={() => nav(`/w/${wsId}/agents/new`)}>
          + {app.t("Configure in an Agent", "在 Agent 中配置")}
        </Button>
      </div>
      <div className="banner info">
        <span>ⓘ</span>
        <span>{app.t(
          "Configured does not mean connected. A Session exposes an MCP server only after its attachment becomes durably active.",
          "“已配置”不代表“已连接”。只有附件持久化激活后，Session 才会显示该 MCP 服务器。",
        )}</span>
      </div>
      {agents.error instanceof Error && <div className="err">{agents.error.message}</div>}
      <DataGrid
        rows={rows}
        columns={columns}
        rowKey={(row) => row.key}
        state={list}
        loading={agents.isLoading}
        filter={(row, query) => [
          row.name,
          row.url,
          row.credential,
          ...row.agents,
        ].some((value) => value?.toLowerCase().includes(query.toLowerCase()))}
        searchPlaceholder={app.t("Filter MCP servers…", "过滤 MCP 服务器…")}
        emptyTitle={app.t("No MCP bindings yet.", "还没有 MCP 绑定。")}
        emptyHint={app.t(
          "Open an Agent, then add a direct server under Build → Skills & MCP.",
          "打开一个 Agent，然后在“构建 → Skills 与 MCP”中添加直接服务器。",
        )}
      />
    </>
  );
}
