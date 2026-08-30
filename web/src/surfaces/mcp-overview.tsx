import { useQuery } from "@tanstack/react-query";
import { Link, useNavigate, useParams } from "react-router";
import { DataGrid, type Column } from "../components/ui/DataGrid";
import { Button, Pill } from "../components/ui";
import { api, ws } from "../lib/api/client";
import type { AgentConfigList } from "../lib/api/types";
import {
  mcpToolsetPolicySummary,
  type McpToolsetPolicySummary,
} from "../lib/agent-toolsets";
import { useApp } from "../lib/app-state";
import { useListState } from "../lib/useListState";
import { visibleAgents } from "../lib/visible-agents";
import {
  MCP_CAPABILITY_COVERAGE,
  type McpSupport,
} from "../lib/mcp-capabilities";

interface McpInventoryRow {
  key: string;
  name: string;
  target: string;
  promptsAsSkills: boolean;
  credential?: string;
  agents: Array<{ id: string; name?: string }>;
  policies: McpToolsetPolicySummary[];
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

function addDistinctPolicy(
  policies: McpToolsetPolicySummary[],
  policy: McpToolsetPolicySummary,
): void {
  if (!policies.some((candidate) =>
    candidate.enabled === policy.enabled
    && candidate.permission === policy.permission
    && candidate.namedOverrides === policy.namedOverrides)) {
    policies.push(policy);
  }
}

function McpProtocolCoverage() {
  const app = useApp();
  const copy: Record<string, { label: [string, string]; detail: [string, string] }> = {
    tools: { label: ["Tools", "Tools"], detail: ["Discover and call namespaced tools.", "发现并调用带命名空间的工具。"] },
    transport: { label: ["Transport", "传输"], detail: ["Streamable HTTP in both directions; sandbox stdio for Agent integrations.", "双向支持 Streamable HTTP；Agent 集成支持 sandbox stdio。"] },
    catalog_updates: { label: ["Catalog updates", "目录更新"], detail: ["Tool changes refresh dynamically; prompt and resource notifications are consumed by the client transport.", "工具变化会动态刷新；Client transport 会消费 Prompt 与 Resource 更新通知。"] },
    progress: { label: ["Progress", "进度"], detail: ["Wire progress is tested. External-server progress is not yet projected into Session Events or Trace.", "协议进度已测试；外部 Server 的进度尚未投影到 Session Event 或 Trace。"] },
    cancellation: { label: ["Cancellation", "取消"], detail: ["Exported calls accept protocol cancellation. Integration drain stops local admission but does not yet prove peer cancellation delivery.", "导出调用接受协议取消；集成 drain 会停止本地接入，但尚未证明已向对端传递取消。"] },
    prompts: { label: ["Prompts", "Prompts"], detail: ["An integration can opt into Prompts as Skills. Awaken's exported server is tools-only.", "集成可选择将 Prompts 作为 Skills；Awaken 导出的 Server 目前仅提供 Tools。"] },
    resources: { label: ["Resources", "Resources"], detail: ["Client list/read exists at transport level, but resources are not yet injected into the Agent journey.", "Client 的 list/read 存在于 transport 层，但 Resources 尚未进入 Agent 使用闭环。"] },
    sampling: { label: ["Sampling", "Sampling"], detail: ["A transport bridge exists, but the product Host does not advertise or configure it.", "存在 transport bridge，但产品 Host 尚未声明或装配。"] },
    elicitation_roots: { label: ["Elicitation and Roots", "Elicitation 与 Roots"], detail: ["Not advertised by the current product composition.", "当前产品装配未声明。"] },
    tasks: { label: ["MCP Tasks extension", "MCP Tasks 扩展"], detail: ["Not advertised or implemented. Awaken Background execution is a separate Runtime capability.", "尚未声明或实现；Awaken 后台执行是独立的 Runtime 能力。"] },
  };
  const supportLabel = (support: McpSupport) => support === "supported"
    ? app.t("Supported", "支持")
    : support === "conditional"
      ? app.t("Conditional", "有条件支持")
      : app.t("Unavailable", "不可用");
  const supportTone = (support: McpSupport) => support === "supported"
    ? "ok" as const
    : support === "conditional"
      ? "warn" as const
      : "neutral" as const;
  return (
    <details className="runtime-capability-summary" open>
      <summary>
        <span><strong>{app.t("MCP support matrix", "MCP 支持矩阵")}</strong><span className="mut"> · 2025-11-25</span></span>
        <span className="mut">{app.t("Product-level behavior", "产品级行为")}</span>
      </summary>
      <div className="runtime-capability-grid">
        {MCP_CAPABILITY_COVERAGE.map((row) => (
          <div className="runtime-capability-row" key={row.id}>
            <div>
              <strong>{app.t(...copy[row.id].label)}</strong>
              <p>{app.t(...copy[row.id].detail)}</p>
            </div>
            <div className="mcp-capability-statuses">
              <span><small>{app.t("Agent integration", "Agent 集成")}</small><Pill tone={supportTone(row.integration)}>{supportLabel(row.integration)}</Pill></span>
              <span><small>{app.t("Exported server", "导出 Server")}</small><Pill tone={supportTone(row.exportedServer)}>{supportLabel(row.exportedServer)}</Pill></span>
            </div>
          </div>
        ))}
      </div>
      <div className="banner info">
        <span>ⓘ</span>
        <span>{app.t(
          "This is Awaken's platform baseline, not a live claim about every configured server. Confirm negotiated capabilities and real behavior in a Session.",
          "这是 Awaken 平台基线，并非对每个已配置 Server 的实时声明；请在 Session 中确认协商能力和真实行为。",
        )}</span>
      </div>
    </details>
  );
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
  for (const agent of visibleAgents(agents.data?.data)) {
    const agentName = typeof agent.name === "string" && agent.name.trim()
      ? agent.name.trim()
      : undefined;
    for (const raw of agent.mcp_servers ?? []) {
      const server = objectOf(raw);
      const name = typeof server.name === "string" ? server.name : "";
      const target = typeof server.url === "string"
        ? server.url
        : server.type === "sandbox_stdio" && typeof server.command === "string"
          ? `sandbox stdio · ${server.command}`
          : "";
      if (!name || !target) continue;
      const key = `${name}\u0000${target}`;
      const policy = mcpToolsetPolicySummary(agent.tools, name);
      const existing = inventory.get(key);
      if (existing) {
        if (!existing.agents.some((item) => item.id === agent.id)) {
          existing.agents.push({ id: agent.id, name: agentName });
        }
        addDistinctPolicy(existing.policies, policy);
      } else {
        inventory.set(key, {
          key,
          name,
          target,
          promptsAsSkills: server.prompts_as_skills === true,
          credential: credentialLabel(server.credential),
          agents: [{ id: agent.id, name: agentName }],
          policies: [policy],
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
          <code className="mut">{row.target}</code>
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
      key: "policy",
      header: app.t("ToolSet policy", "ToolSet 策略"),
      sortValue: (row) => row.policies.length,
      cell: (row) => {
        const policy = row.policies[0];
        if (row.policies.length > 1) {
          return <Pill tone="warn">{app.t(
            `${row.policies.length} Agent policies`,
            `${row.policies.length} 个 Agent 策略`,
          )}</Pill>;
        }
        return (
          <span style={{ display: "flex", flexDirection: "column", gap: 4, alignItems: "flex-start" }}>
            <Pill tone={!policy.enabled ? "neutral" : policy.permission === "always_allow" ? "ok" : "warn"}>
              {!policy.enabled
                ? app.t("Unavailable by default", "默认不可用")
                : policy.permission === "always_allow"
                  ? app.t("Allow without asking", "无需询问即可使用")
                  : app.t("Ask before use", "使用前询问")}
            </Pill>
            {policy.namedOverrides > 0 && <span className="mut">{app.t(
              `${policy.namedOverrides} named override${policy.namedOverrides === 1 ? "" : "s"}`,
              `${policy.namedOverrides} 个指定工具覆盖`,
            )}</span>}
          </span>
        );
      },
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
          {row.agents.map((agent) => (
            <Link key={agent.id} to={`/w/${wsId}/agents/${agent.id}?stage=build&section=integrations`}>
              <Pill tone="info">
                <span style={{ display: "inline-flex", alignItems: "baseline", gap: 6 }}>
                  <span>{agent.name?.trim() || agent.id}</span>
                  {agent.name?.trim() && <code className="mut">{agent.id}</code>}
                </span>
              </Pill>
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
            "This list is built from Agent drafts. Open the owning Agent to change its connection, credential, ToolSet policy, or prompt exposure.",
            "此清单来自 Agent 草稿。需要修改连接、凭证、ToolSet 策略或 Prompt 暴露方式时，请打开对应 Agent。",
          )}{" "}
          <a
            href={`https://awakenworks.com${app.locale === "zh" ? "/zh" : ""}/docs/agents/protocols/mcp/`}
            target="_blank"
            rel="noreferrer"
          >
            {app.t("Review the MCP and ToolSet guide ↗", "查看 MCP 与 ToolSet 指南 ↗")}
          </a>
        </p>
        <Button variant="primary" onClick={() => nav(`/w/${wsId}/agents/new`)}>
          + {app.t("Configure in an Agent", "在 Agent 中配置")}
        </Button>
      </div>
      <div className="banner info">
        <span>ⓘ</span>
        <span>{app.t(
          "Configured means the Agent draft references this server. Confirm the connection in a real Session before relying on its tools.",
          "“已配置”表示 Agent 草稿已引用该服务器。请在真实会话中确认连接后，再依赖其中的工具。",
        )}</span>
      </div>
      <McpProtocolCoverage />
      {agents.error instanceof Error && <div className="err">{agents.error.message}</div>}
      <DataGrid
        rows={rows}
        columns={columns}
        rowKey={(row) => row.key}
        state={list}
        loading={agents.isLoading}
        filter={(row, query) => [
          row.name,
          row.target,
          row.credential,
          ...row.policies.map((policy) => `${policy.enabled} ${policy.permission} ${policy.namedOverrides}`),
          ...row.agents.flatMap((agent) => [agent.name, agent.id]),
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
