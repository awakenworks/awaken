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

type State = "checking" | "ready" | "setup" | "unavailable";

export default function LocalSetupGate({ children }: { children: ReactNode }) {
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
      const entry = hostedSessionEntry(navigation, getToken());
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
      setError(cause instanceof Error ? cause.message : "The setup token is invalid, expired, or already used.");
    }
  }

  if (state === "ready") return children;
  if (state === "checking") return <div className="local-setup-loading">Opening Awaken…</div>;
  if (state === "unavailable") return (
    <main className="local-setup-page">
      <Card>
        <p className="eyebrow">ACCESS UNAVAILABLE</p>
        <h1>Awaken could not verify this browser entry</h1>
        <p className="hint">Retry when the deployment is reachable. No local or Cloud sign-in mode was guessed.</p>
        <Button variant="primary" onClick={() => {
          setState("checking");
          setAttempt((current) => current + 1);
        }}>Try again</Button>
      </Card>
    </main>
  );
  return (
    <main className="local-setup-page">
      <Card>
        <p className="eyebrow">LOCAL ACCESS</p>
        <h1>Connect this browser</h1>
        <p className="hint">Paste the one-time setup token shown by the Awaken CLI. It expires after five minutes and is never stored by this browser.</p>
        <form onSubmit={submit}>
          <label htmlFor="local-setup-token">Setup token</label>
          <input
            id="local-setup-token"
            autoFocus
            autoComplete="off"
            value={token}
            onChange={(event) => setToken(event.target.value)}
          />
          {error && <p className="error">{error}</p>}
          <Button variant="primary" type="submit" disabled={!token.trim()}>Connect</Button>
        </form>
      </Card>
    </main>
  );
}
