// Built-in connection guide for every execution/protocol adapter mounted by the
// production binary. Keep the paths here aligned with router conformance tests.

import { useApp } from "../lib/app-state";
import { Card, Pill } from "../components/ui";
import { useCapabilities } from "../lib/useCapabilities";
import { runtimeStatus } from "../lib/readiness";

export const PROTOCOLS = [
  { id: "managed", name: "Managed Agents", endpoint: "/v1/sessions", mode: "HTTP + SSE", token: "service" },
  { id: "ai-sdk", name: "Vercel AI SDK", endpoint: "/v1/ai-sdk/chat", mode: "UI Message Stream", token: "application" },
  { id: "ag-ui", name: "AG-UI", endpoint: "/v1/ag-ui", mode: "SSE events", token: "application" },
  { id: "a2a", name: "A2A", endpoint: "/v1/a2a", mode: "JSON-RPC / HTTP+JSON", token: "service" },
  { id: "mcp", name: "MCP Server", endpoint: "/v1/mcp", mode: "Streamable HTTP", token: "dedicated" },
] as const;

export const APPLICATION_TOKEN_CURL = `curl http://localhost:8080/v1/application-access-tokens \\
  -H "Authorization: Bearer $AWAKEN_API_KEY" \\
  -H 'content-type: application/json' \\
  -d '{
    "authority_id": "my-backend",
    "application_scope": "project_42",
    "actor_key": "opaque-user-ref",
    "protocols": ["ai-sdk"],
    "operations": ["thread.run", "thread.messages.read"],
    "thread_bindings": [{
      "external_thread_id": "chat_thread_7",
      "managed_session_id": "sesn_123"
    }],
    "expires_in_seconds": 300
  }'`;

export const FRONTEND_AI_SDK = `import { useChat } from "@ai-sdk/react";
import { DefaultChatTransport } from "ai";

// Implement this against YOUR backend. Never put AWAKEN_API_KEY in browser code.
// The backend returns the external id that it bound to an existing Managed Session.
const { access_token, thread_id: threadId } = await getApplicationToken();

const chat = useChat({
  id: threadId,
  transport: new DefaultChatTransport({
    api: \`\${AWAKEN_URL}/v1/ai-sdk/threads/\${threadId}/runs\`,
    headers: { Authorization: \`Bearer \${access_token}\` },
  }),
});`;

export const MANAGED_CURL = `curl http://localhost:8080/v1/sessions \\
  -H "Authorization: Bearer $AWAKEN_API_KEY" \\
  -H 'content-type: application/json' \\
  -d '{"agent":"support","title":"backend job"}'`;

export default function ProtocolsSurface() {
  const app = useApp();
  const capabilities = useCapabilities();
  const runtimes = capabilities.data?.runtimes ?? [];
  return (
    <div className="stack">
      <Card>
        <h2>{app.t("Connect once, choose the client protocol", "一次部署，按客户端选择协议")}</h2>
        <p className="mut">
          {app.t(
            "Every protocol uses the same Agent and Session records. Choose the protocol that fits your client without rebuilding the Agent.",
            "所有协议共用同一套 Agent 和 Session 记录。只需按客户端选择协议，无需重新构建 Agent。",
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
                : protocol.token === "application"
                  ? app.t(
                      "Browser/application route: send a short-lived token limited by protocol, operation, and explicit existing-Session bindings.",
                      "浏览器/应用入口：使用按协议、操作和既有 Session 显式绑定限制的短期令牌。",
                    )
                : app.t(
                    "Server-to-server route: use a workspace-scoped service API key. Never expose it to browser or mobile clients.",
                    "服务间调用入口：使用 workspace 范围的服务 API Key，禁止暴露给浏览器或移动端。",
                  )}
            </p>
          </Card>
        ))}
      </div>
      <Card>
        <h3>{app.t("Backend integration · mint application access", "后端集成 · 签发应用访问令牌")}</h3>
        <p className="mut">
          {app.t(
            "After authorizing the end user, your backend creates or resolves one Managed Session, then asks Awaken for a token bound to that Session. Awaken never needs the user's roles or permissions.",
            "后端完成用户鉴权后，先创建或解析唯一的 Managed Session，再申请绑定该 Session 的令牌。Awaken 无需理解用户角色或权限。",
          )}
        </p>
        <pre className="code-block"><code>{APPLICATION_TOKEN_CURL}</code></pre>
      </Card>
      <Card>
        <h3>{app.t("Frontend integration · Vercel AI SDK", "前端集成 · Vercel AI SDK")}</h3>
        <p className="mut">
          {app.t(
            "Return only the short-lived access token and the bound conversation id to the browser. To continue the same conversation after renewal, bind the new token to the same Managed Session.",
            "只把短期访问令牌和已绑定的对话 ID 返回浏览器。令牌续期后如需继续同一对话，请将新令牌绑定到同一个 Managed Session。",
          )}
        </p>
        <pre className="code-block"><code>{FRONTEND_AI_SDK}</code></pre>
      </Card>
      <Card>
        <h3>{app.t("Backend integration · Managed Agents", "后端集成 · Managed Agents")}</h3>
        <p className="mut">
          {app.t(
            "Trusted services call Managed Agents directly with a service API key. This credential is separate from application access tokens.",
            "可信后端通过服务 API Key 直接调用 Managed Agents；该凭证与应用访问令牌相互分离。",
          )}
        </p>
        <pre className="code-block"><code>{MANAGED_CURL}</code></pre>
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
        <div className="stack" style={{ marginTop: 12 }}>
          {runtimes.map((runtime) => {
            const status = runtime.kind === "native" ? "ready" : runtimeStatus(runtime);
            return (
              <div className="row" key={runtime.id} style={{ justifyContent: "space-between" }}>
                <span>
                  <strong>{runtime.label}</strong>{" "}
                  <code>{runtime.id}</code>
                  {runtime.local?.version && <span className="mut"> · {runtime.local.version}</span>}
                </span>
                <Pill tone={status === "ready" ? "ok" : status === "login_required" ? "warn" : "neutral"}>
                  {status.replaceAll("_", " ")}
                </Pill>
                {runtime.local?.remediation && <span className="hint">{runtime.local.remediation}</span>}
              </div>
            );
          })}
        </div>
      </Card>
    </div>
  );
}
