import { useState } from "react";
import { useNavigate, useParams } from "react-router";
import { getToken, getWorkspace, setToken } from "../lib/api/client";
import { useApp } from "../lib/app-state";
import { Button, Card, SecretField } from "../components/ui";

export default function SettingsSurface() {
  const app = useApp();
  const nav = useNavigate();
  const { ws: wsId = "default" } = useParams();
  const [managementToken, setManagementToken] = useState(getToken());
  const [saved, setSaved] = useState(false);
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
        <Card>
          <h2>{app.t("Console access", "控制台访问")}</h2>
          <p className="hint">
            {app.t(
              "Management bearer for this browser. It is separate from Provider credentials and is never shown in the global chrome.",
              "此浏览器使用的管理令牌。它与 Provider 凭证相互独立，不在全局顶栏展示。",
            )}
          </p>
          <SecretField
            label={app.t("Management bearer", "管理令牌")}
            hasStored={!!getToken()}
            onChange={(intent) => {
              setManagementToken(intent.mode === "replace" ? intent.value ?? "" : intent.mode === "clear" ? "" : getToken());
              setSaved(false);
            }}
          />
          <div className="row" style={{ justifyContent: "flex-end" }}>
            {saved && <span className="mut">{app.t("Saved locally", "已保存到本地")}</span>}
            <Button onClick={() => { setToken(""); setManagementToken(""); setSaved(true); }}>
              {app.t("Clear", "清除")}
            </Button>
            <Button variant="primary" onClick={() => { setToken(managementToken.trim()); setSaved(true); }}>
              {app.t("Save", "保存")}
            </Button>
          </div>
        </Card>
      </div>
    </>
  );
}
