import { useQueryClient } from "@tanstack/react-query";
import { FormEvent, ReactNode, useEffect, useState } from "react";
import {
  ApiClientError,
  AUTHENTICATION_REQUIRED_EVENT,
  api,
  clearProductSessionBearer,
  getToken,
  resolveWorkspaceContext,
} from "../../lib/api/client";
import {
  hostedAccessFailure,
  hostedBootstrapDecision,
  hostedSessionEntry,
  suiteNavigationQuery,
} from "../../lib/suite-navigation";
import { Button, Card } from "../ui";
import { useApp } from "../../lib/app-state";

type State = "checking" | "ready" | "setup" | "denied" | "unavailable";

export default function LocalSetupGate({ children }: { children: ReactNode }) {
  const app = useApp();
  const queryClient = useQueryClient();
  const [state, setState] = useState<State>("checking");
  const [token, setToken] = useState("");
  const [error, setError] = useState("");
  const [requestId, setRequestId] = useState("");
  const [cloudEntryUrl, setCloudEntryUrl] = useState("");
  const [attempt, setAttempt] = useState(0);

  useEffect(() => {
    // Initialization owns its own 401 -> setup transition. Listening while the
    // gate is already checking would turn that expected 401 into a restart
    // loop: /v1/session -> auth event -> initialize -> /v1/session. The global
    // event is only needed after authenticated product screens are mounted.
    if (state !== "ready") return;
    const requireAuthentication = () => {
      queryClient.clear();
      setError("");
      setToken("");
      setState("checking");
      setAttempt((current) => current + 1);
    };
    window.addEventListener(AUTHENTICATION_REQUIRED_EVENT, requireAuthentication);
    return () => window.removeEventListener(AUTHENTICATION_REQUIRED_EVENT, requireAuthentication);
  }, [queryClient, state]);

  useEffect(() => {
    let active = true;
    async function initialize() {
      let navigation;
      try {
        navigation = await queryClient.fetchQuery(suiteNavigationQuery);
      } catch (cause) {
        if (!active) return;
        if (cause instanceof ApiClientError && cause.status === 401) {
          setState("setup");
        } else {
          setState("unavailable");
        }
        return;
      }
      const bootstrap = hostedBootstrapDecision(
        navigation,
        getToken(),
        window.location.href,
        window.location.pathname,
      );
      if (bootstrap.kind === "redirect") {
        window.location.replace(bootstrap.url);
        return;
      }
      if (bootstrap.kind === "verify") {
        try {
          await resolveWorkspaceContext();
          if (active) setState("ready");
        } catch (cause: unknown) {
          if (!active) return;
          const failure = cause instanceof ApiClientError
            ? hostedAccessFailure(cause.status)
            : "unavailable";
          if (failure === "restart") {
            clearProductSessionBearer();
            const restart = hostedSessionEntry(navigation, "", window.location.href);
            if (restart.kind === "redirect") window.location.replace(restart.url);
            return;
          }
          if (failure === "denied" && cause instanceof ApiClientError) {
            const restart = hostedSessionEntry(navigation, "", window.location.href);
            setCloudEntryUrl(restart.kind === "redirect" ? restart.url : navigation.hub_url ?? "");
            setRequestId(cause.requestId ?? "");
            setState("denied");
            return;
          }
          setState("unavailable");
        }
        return;
      }
      try {
        await api.get("/v1/session");
      } catch (cause: unknown) {
        if (!active) return;
        if (cause instanceof ApiClientError && cause.status === 401) {
          setState("setup");
          return;
        }
      }
      try {
        const context = await resolveWorkspaceContext();
        app.setIdentityPresentation({
          workspaceName: context.workspace_display_name,
          organizationName: context.organization_display_name,
          userName: context.user_display_name,
        });
        if (active) setState("ready");
      } catch (cause) {
        if (!active) return;
        setState(cause instanceof ApiClientError && cause.status === 401 ? "setup" : "unavailable");
      }
    }
    void initialize();
    return () => { active = false; };
  }, [attempt, queryClient]);

  async function submit(event: FormEvent) {
    event.preventDefault();
    setError("");
    try {
      await api.post("/v1/auth/local/exchange", { setup_token: token.trim() });
      const context = await resolveWorkspaceContext();
      app.setIdentityPresentation({
        workspaceName: context.workspace_display_name,
        organizationName: context.organization_display_name,
        userName: context.user_display_name,
      });
      setToken("");
      setState("ready");
    } catch (cause) {
      setError(
        cause instanceof ApiClientError && [400, 401, 403].includes(cause.status)
          ? app.t(
              "This setup token is invalid, expired, or already used. Copy the current token from the terminal and try again.",
              "此 Setup Token 无效、已过期或已使用。请从终端复制当前 Token 后重试。",
            )
          : cause instanceof Error
            ? cause.message
            : app.t(
                "Awaken could not authorize this browser. Try again with the current setup token.",
                "Awaken 无法授权此浏览器。请使用当前 Setup Token 重试。",
              ),
      );
    }
  }

  if (state === "ready") return children;
  if (state === "checking") return <div className="local-setup-loading" role="status">{app.t("Checking access to Awaken…", "正在检查 Awaken 访问权限…")}</div>;
  if (state === "unavailable") return (
    <main className="local-setup-page">
      <Card>
        <p className="eyebrow">{app.t("CONNECTION FAILED", "连接失败")}</p>
        <h1>{app.t("Awaken is not reachable", "无法连接 Awaken")}</h1>
        <p className="hint">{app.t("Check that the Awaken service is running and that this address is correct, then try again.", "请确认 Awaken 服务正在运行且当前地址正确，然后重试。")}</p>
        <Button variant="primary" onClick={() => {
          setState("checking");
          setAttempt((current) => current + 1);
        }}>{app.t("Try again", "重试")}</Button>
      </Card>
    </main>
  );
  if (state === "denied") return (
    <main className="local-setup-page">
      <Card>
        <p className="eyebrow">{app.t("ACCESS DENIED", "访问被拒绝")}</p>
        <h1>{app.t("This Workspace is not available to your account", "当前账户无权访问此 Workspace")}</h1>
        <p className="hint">{app.t(
          "Choose another organization or ask an administrator to assign this Awaken Workspace role.",
          "请选择其他组织，或请管理员分配此 Awaken Workspace 角色。",
        )}</p>
        {requestId && <p className="hint">{app.t("Request ID", "请求 ID")}: <code>{requestId}</code></p>}
        <Button variant="primary" onClick={() => window.location.replace(cloudEntryUrl)}>
          {app.t("Choose organization", "选择组织")}
        </Button>
      </Card>
    </main>
  );
  return (
    <main className="local-setup-page">
      <Card>
        <div className="row" style={{ justifyContent: "space-between" }}>
          <p className="eyebrow">{app.t("LOCAL SIGN-IN", "本地登录")}</p>
          <Button variant="ghost" type="button" onClick={app.toggleLocale}>{app.locale === "en" ? "中文" : "EN"}</Button>
        </div>
        <h1>{app.t("Authorize this browser", "授权此浏览器")}</h1>
        <p className="hint">{app.t("Copy the one-time setup token from the terminal that started Awaken. The token expires after five minutes and is used only to authorize this browser.", "复制启动 Awaken 的终端中显示的一次性 Setup Token。它会在五分钟后过期，仅用于授权当前浏览器。")}</p>
        <form onSubmit={submit}>
          <label htmlFor="local-setup-token">{app.t("Setup token", "Setup Token")}</label>
          <input
            id="local-setup-token"
            autoFocus
            autoComplete="off"
            value={token}
            onChange={(event) => setToken(event.target.value)}
          />
          {error && <p className="error" role="alert">{error}</p>}
          <Button variant="primary" type="submit" disabled={!token.trim()}>{app.t("Continue to Awaken", "进入 Awaken")}</Button>
        </form>
      </Card>
    </main>
  );
}
