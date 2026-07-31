import { Card, EmptyState, Pill } from "../ui";
import type { Session } from "../../lib/api/types";
import { useApp } from "../../lib/app-state";

function objectOf(value: unknown): Record<string, unknown> {
  return value && typeof value === "object" && !Array.isArray(value)
    ? value as Record<string, unknown>
    : {};
}

export default function SessionIntegrations({ session }: { session?: Session }) {
  const app = useApp();
  const servers = (session?.agent.mcp_servers ?? []).map(objectOf);
  return (
    <Card style={{ marginTop: 10 }}>
      <div className="row" style={{ justifyContent: "space-between", alignItems: "flex-start" }}>
        <div>
          <h2 className="section-title">{app.t("Active MCP integrations", "已激活的 MCP 集成")}</h2>
          <p className="hint">{app.t(
            "This is the Session's durable active projection, not the Agent's desired configuration.",
            "这里展示 Session 的持久化已激活投影，而不是 Agent 的期望配置。",
          )}</p>
        </div>
        <Pill tone={servers.length > 0 ? "ok" : "neutral"}>
          {servers.length} {app.t("active", "已激活")}
        </Pill>
      </div>
      {servers.length === 0 ? (
        <EmptyState
          title={app.t("No active MCP servers.", "没有已激活的 MCP 服务器。")}
          hint={app.t(
            "A configured server appears here only after Session preparation succeeds. Connection failures remain visible in Trace.",
            "只有 Session 准备成功后，已配置服务器才会显示在这里；连接失败仍可在 Trace 中查看。",
          )}
        />
      ) : (
        <div className="stack">
          {servers.map((server, index) => (
            <div className="row" key={`${String(server.name)}-${index}`} style={{ justifyContent: "space-between" }}>
              <span style={{ display: "flex", flexDirection: "column", gap: 3 }}>
                <strong>{typeof server.name === "string" ? server.name : app.t("MCP server", "MCP 服务器")}</strong>
                <code className="mut">{typeof server.url === "string" ? server.url : "—"}</code>
              </span>
              <span className="row">
                {server.prompts_as_skills === true && (
                  <Pill tone="agent">{app.t("remote Skills", "远程 Skills")}</Pill>
                )}
                <Pill tone="ok">{app.t("active", "已激活")}</Pill>
              </span>
            </div>
          ))}
        </div>
      )}
    </Card>
  );
}
