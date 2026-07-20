// Built-in connection guide for every execution/protocol adapter mounted by the
// production binary. Keep the paths here aligned with router conformance tests.

import { useApp } from "../lib/app-state";
import { Card, Pill } from "../components/ui";

export const PROTOCOLS = [
  { id: "managed", name: "Managed Agents", endpoint: "/v1/sessions", mode: "HTTP + SSE", token: "console" },
  { id: "ai-sdk", name: "Vercel AI SDK", endpoint: "/v1/ai-sdk/chat", mode: "Data Stream", token: "console" },
  { id: "ag-ui", name: "AG-UI", endpoint: "/v1/ag-ui", mode: "SSE events", token: "console" },
  { id: "a2a", name: "A2A", endpoint: "/v1/a2a", mode: "JSON-RPC / HTTP+JSON", token: "console" },
  { id: "mcp", name: "MCP Server", endpoint: "/v1/mcp", mode: "Streamable HTTP", token: "dedicated" },
] as const;

const CURL = `curl -N http://localhost:8080/v1/ai-sdk/chat \\
  -H 'content-type: application/json' \\
  -d '{"id":"demo","messages":[{"role":"user","content":"Summarize this release"}]}'`;

export default function ProtocolsSurface() {
  const app = useApp();
  return (
    <div className="stack">
      <Card>
        <h2>{app.t("Connect once, choose the client protocol", "一次部署，按客户端选择协议")}</h2>
        <p className="mut">
          {app.t(
            "All adapters drive the same durable thread runtime. A thread started through one protocol can be observed through another without duplicating Agent logic.",
            "所有适配器驱动同一个持久化 thread 运行时。通过一种协议启动的 thread 可由另一种协议观察，无需复制 Agent 逻辑。",
          )}
        </p>
      </Card>
      <div className="grid-2">
        {PROTOCOLS.map((protocol) => (
          <Card key={protocol.id}>
            <div className="row" style={{ justifyContent: "space-between" }}>
              <h3>{protocol.name}</h3>
              <Pill tone="neutral">{protocol.mode}</Pill>
            </div>
            <code>{protocol.endpoint}</code>
            <p className="hint">
              {protocol.token === "dedicated"
                ? app.t(
                    "Set AWAKEN_MCP_BEARER_TOKEN; the route is absent until configured. Send it as Authorization: Bearer.",
                    "设置 AWAKEN_MCP_BEARER_TOKEN；配置前路由不存在。通过 Authorization: Bearer 发送。",
                  )
                : app.t(
                    "Local self-hosted mode is open by default. Management IAM uses the bearer saved in the top bar; protect public protocol ingress at your gateway.",
                    "本地自托管默认开放。管理 IAM 使用顶部保存的 bearer；公网协议入口需在网关保护。",
                  )}
            </p>
          </Card>
        ))}
      </div>
      <Card>
        <h3>{app.t("AI SDK quick start", "AI SDK 快速开始")}</h3>
        <pre className="code-block"><code>{CURL}</code></pre>
      </Card>
      <Card>
        <h3>{app.t("Execution adapters", "执行适配器")}</h3>
        <p className="mut">
          <strong>ACP</strong> — {app.t("select an Environment with runtime acp:claude, acp:codex, or a custom ACP CLI.", "选择 runtime 为 acp:claude、acp:codex 或自定义 ACP CLI 的 Environment。")}
        </p>
        <p className="mut">
          <strong>Sandbox</strong> — {app.t("the Environment config applies isolation, network and resource limits to native and ACP execution.", "Environment 配置将隔离、网络和资源限制同时应用到原生与 ACP 执行。")}
        </p>
        <p className="mut">
          <strong>A2A discovery</strong> — <code>/.well-known/agent-card.json</code> · <code>/v1/a2a/message:send</code> · <code>/v1/a2a/message:stream</code>
        </p>
      </Card>
    </div>
  );
}
