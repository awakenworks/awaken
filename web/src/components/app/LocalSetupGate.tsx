import { useQueryClient } from "@tanstack/react-query";
import { FormEvent, ReactNode, useEffect, useState } from "react";
import {
  ApiClientError,
  api,
  getToken,
  resolveWorkspaceContext,
} from "../../lib/api/client";
import {
  hostedSessionEntry,
  suiteNavigationQuery,
} from "../../lib/suite-navigation";
import { Button, Card } from "../ui";
import { useApp } from "../../lib/app-state";

type State = "checking" | "ready" | "setup" | "unavailable";

export default function LocalSetupGate({ children }: { children: ReactNode }) {
  const app = useApp();
  const queryClient = useQueryClient();
  const [state, setState] = useState<State>("checking");
  const [token, setToken] = useState("");
  const [error, setError] = useState("");
  const [attempt, setAttempt] = useState(0);

  useEffect(() => {
    let active = true;
    async function initialize() {
      let navigation;
      try {
        navigation = await queryClient.fetchQuery(suiteNavigationQuery);
      } catch {
        if (active) setState("unavailable");
        return;
      }
      const entry = hostedSessionEntry(navigation, getToken(), window.location.href);
      if (entry.kind === "redirect") {
        window.location.replace(entry.url);
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
        await resolveWorkspaceContext();
        if (active) setState("ready");
      } catch {
        if (active) setState("unavailable");
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
      await resolveWorkspaceContext();
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
