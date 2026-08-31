// Built-in connection guide for every execution/protocol adapter mounted by the
// production binary. Keep the paths here aligned with router conformance tests.

import { useApp } from "../lib/app-state";
import { Link, useParams, useSearchParams } from "react-router";
import { Card, CopyButton, Pill } from "../components/ui";
import { useCapabilities } from "../lib/useCapabilities";
import { useConfigCapabilities } from "../lib/useConfigCapabilities";
import { runtimeStatus } from "../lib/readiness";

export const PROTOCOLS = [
  { id: "managed", name: "Managed Agents", endpoint: "/v1/sessions", mode: "HTTP + SSE", token: "service", docs: "managed-agents", useWhen: ["A trusted backend must start durable Agent work and inspect the same Session later.", "可信后端需要启动持久 Agent 工作，并在之后检查同一个 Session。"] },
  { id: "ai-sdk", name: "Vercel AI SDK", endpoint: "/v1/ai-sdk/chat", mode: "UI Message Stream", token: "application", docs: "ai-sdk", useWhen: ["A web product already uses Vercel AI SDK message and streaming primitives.", "Web 产品已经使用 Vercel AI SDK 的消息与流式交互能力。"] },
  { id: "ag-ui", name: "AG-UI", endpoint: "/v1/ag-ui", mode: "SSE events", token: "application", docs: "ag-ui", useWhen: ["An Agent UI needs AG-UI events, state, and CopilotKit-compatible transport.", "Agent UI 需要 AG-UI 事件、状态或兼容 CopilotKit 的传输。"] },
  { id: "a2a", name: "A2A", endpoint: "/v1/a2a", mode: "JSON-RPC / HTTP+JSON", token: "service", docs: "a2a", useWhen: ["Another Agent must discover Awaken and exchange federated task state.", "其他 Agent 需要发现 Awaken，并交换联邦任务状态。"] },
  { id: "mcp", name: "MCP Server", endpoint: "/v1/mcp", mode: "Streamable HTTP", token: "dedicated", docs: "mcp", useWhen: ["An MCP client must call an explicitly exported set of Awaken tools.", "MCP 客户端需要调用 Awaken 明确导出的工具集合。"] },
] as const;

export const PROTOCOL_EXTENSIONS = [
  {
    id: "acp",
    name: "ACP runtimes",
    endpoint: "Agent model runtime",
    docs: "acp",
    useWhen: [
      "A supported coding-agent CLI should act as the Brain while Awaken retains Session, permission, Sandbox, and commit control.",
      "需要由受支持的 coding-agent CLI 充当 Brain，同时由 Awaken 保留 Session、权限、Sandbox 与提交控制。",
    ],
    guidance: [
      "Choose an acp:<id> model runtime on the Agent, then choose its Sandbox tier independently.",
      "在 Agent 上选择 acp:<id> 模型运行时，再单独选择其 Sandbox tier。",
    ],
  },
  {
    id: "live-inbox",
    name: "Live Inbox",
    endpoint: "/v1/awaken/sessions/{session_id}/live-inbox",
    docs: "live-inbox",
    useWhen: [
      "An application must edit input that an active native Run has not consumed yet.",
      "应用需要编辑一个活跃 Native Run 尚未消费的输入。",
    ],
    guidance: [
      "Use durable Session events when no native Run is active, the input must be retained, or the executor is ACP.",
      "没有活跃 Native Run、输入必须持久保留或 executor 为 ACP 时，请使用持久 Session event。",
    ],
  },
] as const;

type ProtocolId = typeof PROTOCOLS[number]["id"];

export function protocolDocsUrl(locale: "en" | "zh", slug: string) {
  const prefix = locale === "zh" ? "/zh" : "";
  return `https://awakenworks.com${prefix}/docs/agents/protocols/${slug}/`;
}
interface ProtocolHelp {
  credential: string;
  steps: readonly [string, string, string];
  example?: string;
  href?: string;
  linkLabel?: readonly [string, string];
}

export const APPLICATION_TOKEN_CURL = `curl "$AWAKEN_URL/v1/application-access-tokens" \\
  -H "Authorization: Bearer $AWAKEN_API_KEY" \\
  -H 'content-type: application/json' \\
  -d '{
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

export interface ManagedSdkContext {
  agentId?: string | null;
  environmentId?: string | null;
  workspaceId?: string | null;
}

function sdkCoordinate(value: string | null | undefined, environmentVariable: string): string {
  return value?.trim() ? JSON.stringify(value) : `process.env.${environmentVariable}`;
}

/** Derive a contextual example from one SDK template. Query coordinates only
 * replace two copy-time values and never become configuration authority. */
export function managedSdkExample(context: ManagedSdkContext = {}): string {
  const agent = sdkCoordinate(context.agentId, "AWAKEN_AGENT_ID");
  const environment = sdkCoordinate(context.environmentId, "AWAKEN_ENVIRONMENT_ID");
  const workspace = encodeURIComponent(context.workspaceId?.trim() || "default");
  return `// npm install @anthropic-ai/sdk
import Anthropic from "@anthropic-ai/sdk";

const betas = ["managed-agents-2026-04-01"];

const awaken = new Anthropic({
  apiKey: process.env.AWAKEN_API_KEY,
  baseURL: process.env.AWAKEN_BASE_URL,
});

const session = await awaken.beta.sessions.create({
  agent: ${agent},
  environment_id: ${environment},
  betas,
});

await awaken.beta.sessions.events.send(session.id, {
  events: [{
    type: "user.message",
    content: [{ type: "text", text: "Review this support escalation." }],
  }],
  betas,
});

console.log(\`Open in Console: /w/${workspace}/sessions/\${session.id}\`);`;
}

export const MANAGED_SDK = managedSdkExample();

export const PROTOCOL_HELP: Record<ProtocolId, ProtocolHelp> = {
  managed: {
    credential: "Workspace service API key",
    steps: [
      "Under Access, create a dedicated Workspace administrator service API key so this client can create Sessions, then copy its one-time secret.",
      "Point the official Anthropic SDK at Awaken from your trusted backend. Never expose this key to browser or mobile code.",
      "Send events and inspect the same Session in Awaken.",
    ],
    example: MANAGED_SDK,
    href: "access",
    linkLabel: ["Create or manage service API keys →", "创建或管理 Service API Key →"],
  },
  "ai-sdk": { credential: "Application access token", steps: ["Your backend resolves one Managed Session.", "Mint a short-lived token bound to that Session.", "Return only the token and external thread id to the browser."], example: FRONTEND_AI_SDK },
  "ag-ui": { credential: "Application access token", steps: ["Your backend resolves one Managed Session.", "Mint a short-lived token with the ag-ui protocol and required operations.", "Connect the AG-UI client with the token; renew against the same Session."], example: APPLICATION_TOKEN_CURL.replace('["ai-sdk"]', '["ag-ui"]') },
  a2a: { credential: "Service API key", steps: ["Publish the Agent that should receive federated work.", "Let peers discover this deployment from its public Agent Card.", "Send or stream a message, then inspect the resulting Session."], href: "a2a-servers" },
  mcp: { credential: "Dedicated MCP bearer token", steps: ["Set AWAKEN_MCP_BEARER_TOKEN before startup.", "Connect the MCP client to the Streamable HTTP endpoint.", "Run a tool call and confirm it in the owning Session Trace."], href: "mcp" },
} as const;

function internalHelpLabel(
  help: ProtocolHelp,
  protocolName: string,
  t: (en: string, zh: string) => string,
) {
  const label = help.linkLabel;
  return label
    ? t(label[0], label[1])
    : t(`Open ${protocolName} help and status →`, `打开 ${protocolName} 帮助与状态 →`);
}

export function managedCredentialLabel(
  accessManagement: boolean,
  identityMode: string | undefined,
  locale: "en" | "zh",
) {
  if (accessManagement) return locale === "zh" ? "Workspace Service API Key" : "Workspace service API key";
  if (identityMode === "no-login") return locale === "zh" ? "本地 no-login 模式无需 Key" : "No key required in local no-login mode";
  return locale === "zh" ? "当前部署未开放 Service API Key" : "Service API keys unavailable in this deployment";
}

function connectionSteps(
  protocol: typeof PROTOCOLS[number],
  locale: "en" | "zh",
  accessManagement: boolean,
  identityMode: string | undefined,
) {
  if (protocol.id === "managed" && !accessManagement) {
    if (locale === "zh") return identityMode === "no-login"
      ? ["本地 no-login 模式不要求 API Key。", "需要鉴权时，以 self-managed 身份模式启动 Awaken，再在“访问控制”中创建限定 Workspace 的 Service API Key。", "从可信后端将 Anthropic 官方 SDK 指向 Awaken，并在 Console 检查同一个 Session。"]
      : ["当前部署未开放 Service API Key 管理。", "需要长期后端凭据时，以 self-managed 身份模式启动 Awaken，再在“访问控制”中创建 Key。", "从可信后端将 Anthropic 官方 SDK 指向 Awaken，并在 Console 检查同一个 Session。"];
    return identityMode === "no-login"
      ? ["Local no-login mode does not require an API key.", "For authenticated access, restart Awaken in self-managed identity mode and create a Workspace service API key under Access.", "Point the official Anthropic SDK at Awaken from your trusted backend, then inspect the same Session in Console."]
      : ["This deployment does not expose service API key management.", "For a long-lived backend credential, restart Awaken in self-managed identity mode and create the key under Access.", "Point the official Anthropic SDK at Awaken from your trusted backend, then inspect the same Session in Console."];
  }
  if (locale !== "zh") return [...PROTOCOL_HELP[protocol.id].steps];
  return [
    ["在“访问控制”中创建限定 Workspace 的 Service API Key，并立即复制仅显示一次的明文。", "从可信后端将 Anthropic 官方 SDK 指向 Awaken；不要在浏览器或移动端暴露该 Key。", "发送事件，并在 Awaken 中检查同一个 Session。"],
    ["后端解析唯一的 Managed Session。", "签发绑定到该 Session 的短期令牌。", "只向浏览器返回令牌和外部 thread id。"],
    ["后端解析唯一的 Managed Session。", "签发允许 ag-ui 协议及所需操作的短期令牌。", "客户端携令牌连接；续期仍绑定同一个 Session。"],
    ["发布接收联邦任务的 Agent。", "让对端通过公开 Agent Card 发现当前部署。", "发送或流式发送消息，再检查生成的 Session。"],
    ["启动前设置 AWAKEN_MCP_BEARER_TOKEN。", "将 MCP 客户端连接到 Streamable HTTP 入口。", "执行一次工具调用，并在所属 Session Trace 中确认。"],
  ][PROTOCOLS.findIndex((item) => item.id === protocol.id)];
}

export default function ProtocolsSurface() {
  const app = useApp();
  const capabilities = useCapabilities();
  const configCapabilities = useConfigCapabilities();
  const { ws = "default" } = useParams();
  const [searchParams] = useSearchParams();
  const runtimes = capabilities.data?.runtimes ?? [];
  const accessManagement = configCapabilities.data?.surfaces.access_management === true;
  const identityMode = configCapabilities.data?.identity.mode;
  const protocolHelp = {
    ...PROTOCOL_HELP,
    managed: {
      ...PROTOCOL_HELP.managed,
      example: managedSdkExample({
        agentId: searchParams.get("agent"),
        environmentId: searchParams.get("environment"),
        workspaceId: ws,
      }),
    },
  };
  return (
    <div className="stack">
      <Card>
        <h2>{app.t("Choose a protocol, then open its connection help", "选择协议，再查看对应连接帮助")}</h2>
        <p className="mut">
          {app.t(
            "This page is the protocol directory. Every protocol uses the same published Agents and durable Session records; connection steps, credentials, and proof are kept with the protocol they belong to.",
            "这里是协议目录。所有协议共用已发布的 Agent 和持久 Session 记录；连接步骤、凭据边界和验证方法都放在所属协议的帮助中。",
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
            <p className="mut">{app.t(protocol.useWhen[0], protocol.useWhen[1])}</p>
            <p className="hint">{app.t("Credential", "凭据")} · {protocol.id === "managed"
              ? managedCredentialLabel(accessManagement, identityMode, app.locale)
              : protocolHelp[protocol.id].credential}</p>
            <details id={`protocol-${protocol.id}`} className="protocol-help" open={protocol.id === "managed" ? true : undefined}>
              <summary>{app.t(`How to connect ${protocol.name}`, `${protocol.name} 如何连接`)}</summary>
              <ol>
                {connectionSteps(protocol, app.locale, accessManagement, identityMode).map((step) => (
                  <li key={step}>{step}</li>
                ))}
              </ol>
              {protocolHelp[protocol.id].example && (
                <div className="protocol-example">
                  <div className="row" style={{ justifyContent: "flex-end" }}>
                    <CopyButton
                      value={protocolHelp[protocol.id].example!}
                      label={app.t("Copy example", "复制示例")}
                      copiedLabel={app.t("Copied", "已复制")}
                    />
                  </div>
                  <pre className="code-block"><code>{protocolHelp[protocol.id].example}</code></pre>
                </div>
              )}
              {protocolHelp[protocol.id].href && (protocol.id !== "managed" || accessManagement) && (
                <Link className="protocol-help-link" to={`/w/${ws}/${protocolHelp[protocol.id].href}`}>
                  {internalHelpLabel(protocolHelp[protocol.id], protocol.name, app.t)}
                </Link>
              )}
              {protocol.id === "managed" && (
                <a
                  className="protocol-help-link"
                  href={app.locale === "zh" ? "https://awakenworks.com/zh/docs/agents/get-started/" : "https://awakenworks.com/docs/agents/get-started/"}
                  target="_blank"
                  rel="noreferrer"
                >
                  {app.t("Run the complete Getting Started guide ↗", "运行完整 Getting Started 指南 ↗")}
                </a>
              )}
            </details>
            <a
              className="protocol-help-link"
              href={protocolDocsUrl(app.locale, protocol.docs)}
              target="_blank"
              rel="noreferrer"
            >
              {app.t(`Read ${protocol.name} documentation ↗`, `阅读 ${protocol.name} 文档 ↗`)}
            </a>
          </Card>
        ))}
      </div>
      <Card>
        <h3>{app.t("Execution and interaction extensions", "执行与交互扩展")}</h3>
        <div className="grid-2">
          {PROTOCOL_EXTENSIONS.map((extension) => (
            <section className="protocol-extension" key={extension.id}>
              <div className="row" style={{ justifyContent: "space-between" }}>
                <strong>{extension.name}</strong>
                <code>{extension.endpoint}</code>
              </div>
              <p className="mut">{app.t(extension.useWhen[0], extension.useWhen[1])}</p>
              <p className="hint">{app.t(extension.guidance[0], extension.guidance[1])}</p>
              <a
                className="protocol-help-link"
                href={protocolDocsUrl(app.locale, extension.docs)}
                target="_blank"
                rel="noreferrer"
              >
                {app.t(`Read ${extension.name} documentation ↗`, `阅读 ${extension.name} 文档 ↗`)}
              </a>
            </section>
          ))}
        </div>
        <p className="mut">
          <strong>Sandbox</strong> — {app.t("the Environment config applies isolation, network and resource limits to native and ACP execution.", "Environment 配置将隔离、网络和资源限制同时应用到原生与 ACP 执行。")}
        </p>
        <p className="mut">
          <strong>A2A discovery</strong> — <code>/.well-known/agent-card.json</code> · <code>/v1/a2a/message:send</code> · <code>/v1/a2a/message:stream</code>
        </p>
        <section className="protocol-extension">
          <div className="row" style={{ justifyContent: "space-between" }}>
            <strong>{app.t("Outbound lifecycle notifications", "出站生命周期通知")}</strong>
            <code>/v1/config/webhook-subscriptions</code>
          </div>
          <p className="mut">{app.t("Use signed Webhooks when your backend needs major Agent and Session state changes without polling. This direction is Awaken → your backend; it does not call an Agent.", "后端需要无需轮询地接收重要 Agent 与 Session 状态变化时，请使用签名 Webhook。方向是 Awaken → 你的后端，不用于调用 Agent。")}</p>
          <Link className="protocol-help-link" to={`/w/${ws}/webhooks`}>
            {app.t("Create and monitor Webhooks →", "创建并监控 Webhook →")}
          </Link>
        </section>
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
                  {status === "ready"
                    ? app.t("ready", "就绪")
                    : status === "login_required"
                      ? app.t("login required", "需要登录")
                      : app.t("not detected", "未检测到")}
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
