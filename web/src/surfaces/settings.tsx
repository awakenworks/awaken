import { useNavigate, useParams } from "react-router";
import { getWorkspace } from "../lib/api/client";
import { useApp } from "../lib/app-state";
import { Card } from "../components/ui";

export default function SettingsSurface() {
  const app = useApp();
  const nav = useNavigate();
  const { ws: wsId = "default" } = useParams();
  const base = `/w/${wsId}`;
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
          {app.t("Workspace scope", "工作区作用域")} · <code>{wsId}</code> ·{" "}
          {getWorkspace()
            ? app.t("explicit management path", "显式管理路径")
            : app.t("local flat API", "本地扁平 API")}
        </span>
      </div>
      <div className="settings-grid">
        <Card>
          <h2>{app.t("Configuration", "配置")}</h2>
          {link(app.t("Providers & models", "供应商与模型"), `${base}/models`, app.t("Connect once, verify, and discover models", "一次连接、验证并发现模型"))}
          {link(app.t("Inference credentials", "推理凭证"), `${base}/credentials`, app.t("Secret-free source status and Claude setup token", "无秘密状态及 Claude setup token"))}
          {link(app.t("Environments", "运行环境"), `${base}/environments`, app.t("Execution placement and networking", "执行位置与网络"))}
          {link(app.t("Runtime secrets", "运行秘密"), `${base}/vaults`, app.t("Tool and integration secrets; not model credentials", "工具与集成秘密，不含模型凭证"))}
        </Card>
      </div>
    </>
  );
}
