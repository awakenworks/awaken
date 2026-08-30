import { useNavigate, useParams } from "react-router";
import { useApp } from "../lib/app-state";
import { Card } from "../components/ui";
import { useConfigCapabilities } from "../lib/useConfigCapabilities";
import { hasSurface } from "../lib/navigation/paths";

export default function SettingsSurface() {
  const app = useApp();
  const nav = useNavigate();
  const { ws: wsId = "default" } = useParams();
  const base = `/w/${wsId}`;
  const capabilities = useConfigCapabilities();
  const byokEnabled = capabilities.data?.models.byok_enabled === true;
  const managedRuntime = hasSurface(capabilities.data, "managed_runtime");
  const accessManagement = hasSurface(capabilities.data, "access_management");
  const link = (label: string, to: string, hint: string) => (
    <button className="settings-link" onClick={() => nav(to)}>
      <span>
        <strong>{label}</strong>
        <small>{hint}</small>
      </span>
      <span>↗</span>
    </button>
  );
  return (
    <>
      <div className="banner info">
        <span>⚑</span>
        <span>
          {app.t("Changes made from the linked pages apply only to", "通过下方页面进行的配置只作用于")} <strong>{app.workspaceName}</strong>
        </span>
      </div>
      <div className="settings-grid">
        <Card>
          <h2>{app.t("Configuration", "配置")}</h2>
          {link(
            app.t(byokEnabled ? "Providers & models" : "Models", byokEnabled ? "供应商与模型" : "模型"),
            `${base}/models`,
            app.t(
              byokEnabled ? "Connect once, verify, and discover models" : "Browse every model available from Awaken Cloud",
              byokEnabled ? "一次连接、验证并发现模型" : "浏览 Awaken Cloud 提供的全部模型",
            ),
          )}
          {byokEnabled && link(app.t("Inference credentials", "推理凭证"), `${base}/credentials`, app.t("Credential status without secret values, plus Claude Setup Token", "凭证状态（不显示秘密值）与 Claude Setup Token"))}
          {managedRuntime && link(app.t("Environments", "运行环境"), `${base}/environments`, app.t("Packages, placement, networking, limits, and Sandbox timing", "软件包、运行位置、网络、资源限制和 Sandbox 时机"))}
          {managedRuntime && link(app.t("Runtime secrets", "运行时凭证"), `${base}/vaults`, app.t("Tool and integration credentials; not model credentials", "工具与集成凭证，不含模型凭证"))}
        </Card>
        {managedRuntime && <Card>
          <h2>{app.t("Connections & access", "连接与访问")}</h2>
          {link(app.t("API & protocols", "API 与协议"), `${base}/protocols`, app.t("Choose the client, credential type, endpoint, and runnable example", "选择客户端、凭据类型、端点与可运行示例"))}
          {link(app.t("MCP overview", "MCP 概览"), `${base}/mcp`, app.t("Inspect Agent-owned MCP integrations and ToolSet policy", "检查 Agent 管理的 MCP 集成与 ToolSet 策略"))}
          {link(app.t("Webhooks", "Webhooks"), `${base}/webhooks`, app.t("Send signed lifecycle events and monitor delivery failures", "发送已签名生命周期事件并监控投递失败"))}
          {link(app.t("A2A federation", "A2A 联邦"), `${base}/a2a-servers`, app.t("Inspect the public Agent Card and federated message endpoints", "检查公开 Agent Card 与联邦消息端点"))}
          {accessManagement && link(app.t("Service API keys", "Service API Key"), `${base}/access`, app.t("Create scoped credentials for trusted backends", "为可信后端创建限定范围的凭据"))}
        </Card>}
      </div>
    </>
  );
}
