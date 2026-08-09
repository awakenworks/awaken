import { useNavigate, useParams } from "react-router";
import { useApp } from "../lib/app-state";
import { Card } from "../components/ui";
import { useConfigCapabilities } from "../lib/useConfigCapabilities";

export default function SettingsSurface() {
  const app = useApp();
  const nav = useNavigate();
  const { ws: wsId = "default" } = useParams();
  const base = `/w/${wsId}`;
  const capabilities = useConfigCapabilities();
  const byokEnabled = capabilities.data?.models.byok_enabled === true;
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
          {app.t("Changes made from the linked pages apply to Workspace", "通过下方页面进行的配置只作用于工作区")} · <code>{wsId}</code>
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
          {byokEnabled && link(app.t("Inference credentials", "推理凭证"), `${base}/credentials`, app.t("Secret-free source status and Claude setup token", "无秘密状态及 Claude setup token"))}
          {link(app.t("Environments", "运行环境"), `${base}/environments`, app.t("Packages, placement, networking, limits, and Sandbox timing", "软件包、运行位置、网络、资源限制和 Sandbox 时机"))}
          {link(app.t("Runtime secrets", "运行秘密"), `${base}/vaults`, app.t("Tool and integration secrets; not model credentials", "工具与集成秘密，不含模型凭证"))}
        </Card>
      </div>
    </>
  );
}
