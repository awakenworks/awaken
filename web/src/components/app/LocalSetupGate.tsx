import { FormEvent, ReactNode, useEffect, useState } from "react";
import { ApiClientError, api } from "../../lib/api/client";
import { Button, Card } from "../ui";

type State = "checking" | "ready" | "setup";

export default function LocalSetupGate({ children }: { children: ReactNode }) {
  const [state, setState] = useState<State>("checking");
  const [token, setToken] = useState("");
  const [error, setError] = useState("");

  useEffect(() => {
    api.get("/v1/session")
      .then(() => setState("ready"))
      .catch((cause: unknown) => {
        if (cause instanceof ApiClientError && cause.status === 401) setState("setup");
        else setState("ready");
      });
  }, []);

  async function submit(event: FormEvent) {
    event.preventDefault();
    setError("");
    try {
      await api.post("/v1/auth/local/exchange", { setup_token: token.trim() });
      setToken("");
      setState("ready");
    } catch {
      setError("The setup token is invalid, expired, or already used.");
    }
  }

  if (state === "ready") return children;
  if (state === "checking") return <div className="local-setup-loading">Opening Awaken…</div>;
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
