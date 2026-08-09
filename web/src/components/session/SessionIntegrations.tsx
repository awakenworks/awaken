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
          <h2 className="section-title">{app.t("MCP connections used by this Session", "本次 Session 使用的 MCP 连接")}</h2>
          <p className="hint">{app.t(
            "Only MCP servers that connected successfully for this Session appear here.",
            "这里只显示为当前 Session 成功连接的 MCP 服务器。",
          )}</p>
        </div>
        <Pill tone={servers.length > 0 ? "ok" : "neutral"}>
          {servers.length} {app.t("connected", "已连接")}
        </Pill>
      </div>
      {servers.length === 0 ? (
        <EmptyState
          title={app.t("No MCP servers connected.", "没有已连接的 MCP 服务器。")}
          hint={app.t(
            "Configured servers appear after the Session starts successfully. Open Trace to diagnose a failed connection.",
            "Session 成功启动后，已配置的服务器才会显示；连接失败时可打开“追踪”查看原因。",
          )}
        />
      ) : (
        <div className="stack">
          {servers.map((server, index) => (
            <div className="row" key={`${String(server.name)}-${index}`} style={{ justifyContent: "space-between" }}>
              <span style={{ display: "flex", flexDirection: "column", gap: 3 }}>
                <strong>{typeof server.name === "string" ? server.name : app.t("MCP server", "MCP 服务器")}</strong>
                <code className="mut">
                  {typeof server.url === "string"
                    ? server.url
                    : server.type === "sandbox_stdio" && typeof server.command === "string"
                      ? `sandbox stdio · ${server.command}`
                      : "—"}
                </code>
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
