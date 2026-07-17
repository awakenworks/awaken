// In-editor Sandbox: spin up a scratch session bound to the published agent and
// mount the shared <Transcript> — the same engine the session detail uses — so
// "define an agent → test it live, right here" is one continuous flow. Token
// usage comes from the session (real, accumulated); latency is measured client-side.

import { useMutation, useQuery } from "@tanstack/react-query";
import { useState } from "react";
import { api, ws } from "../../lib/api/client";
import type { Environment, Page, Session } from "../../lib/api/types";
import { useApp } from "../../lib/app-state";
import { Button, Card, EmptyState, Pill, UsageBadges } from "../ui";
import Transcript from "./Transcript";

/** Enabled only for a published, saved agent — an unpublished draft is not
 * installed in the runtime, so there is nothing live to talk to yet. */
export default function SandboxPane({
  agentId,
  ready,
  dirty,
}: {
  agentId: string;
  /** The agent is published (installed) and thus runnable. */
  ready: boolean;
  /** The draft has unsaved edits (the live agent lags the form). */
  dirty: boolean;
}) {
  const app = useApp();
  const [sid, setSid] = useState<string | null>(null);
  const [latencyMs, setLatencyMs] = useState<number | undefined>();
  const [envId, setEnvId] = useState("");

  // The environments the scratch run can execute in — picking one that carries an ACP
  // runtime + sandbox is how you test the agent AS claude/codex inside its bwrap, not
  // just native (the two axes — agent × environment — meet here, at run time).
  const envs = useQuery({
    queryKey: ["environments"],
    queryFn: () => api.get<Page<Environment>>(ws("/v1/environments")),
  });
  const chosen = (envs.data?.data ?? []).find((e) => e.id === envId);

  const start = useMutation({
    mutationFn: () => {
      const runtime = chosen?.config.runtime;
      return api.post<Session>(ws("/v1/sessions"), {
        agent: agentId,
        title: `sandbox · ${agentId}`,
        ...(envId ? { environment_id: envId } : {}),
        // The environment carries the runtime; declare it on the session metadata the
        // host reads (same wiring as the Sessions surface).
        ...(runtime ? { metadata: { "awaken.runtime": runtime } } : {}),
      });
    },
    onSuccess: (s) => {
      setSid(s.id);
      setLatencyMs(undefined);
    },
  });

  // Poll the scratch session for accumulated token usage.
  const session = useQuery({
    queryKey: ["sandbox-session", sid],
    enabled: !!sid,
    queryFn: () => api.get<Session>(ws(`/v1/sessions/${sid}`)),
    refetchInterval: 4_000,
  });

  if (!ready) {
    return (
      <EmptyState
        title={app.t("Publish to test in the Sandbox", "发布后即可在 Sandbox 试运行")}
        hint={app.t(
          "The Sandbox runs the installed agent. Save, then Publish, and a live session opens here.",
          "Sandbox 运行已安装的 agent。先保存,再发布,这里就会打开一个实时会话。",
        )}
      />
    );
  }

  if (!sid) {
    return (
      <EmptyState
        title={app.t("Start a live session", "开始一个实时会话")}
        hint={app.t(
          "Opens a scratch session against the published agent — ask it something and watch it answer.",
          "对已发布的 agent 开一个临时会话——问它点什么,看它实时作答。",
        )}
        action={
          <div className="col" style={{ gap: 8, alignItems: "center" }}>
            <label className="row" style={{ gap: 6, fontSize: 12 }}>
              <span className="mut">{app.t("Environment", "运行环境")}</span>
              <select className="input mono" style={{ height: 28 }} value={envId} onChange={(e) => setEnvId(e.target.value)}>
                <option value="">{app.t("Native (no sandbox)", "原生(无沙箱)")}</option>
                {(envs.data?.data ?? []).map((e) => (
                  <option key={e.id} value={e.id}>
                    {e.name}
                    {e.config.runtime ? ` · ${e.config.runtime}` : ""}
                    {e.config.sandbox ? " · sandboxed" : ""}
                  </option>
                ))}
              </select>
            </label>
            <Button variant="primary" disabled={start.isPending} onClick={() => start.mutate()}>
              {start.isPending ? app.t("Starting…", "启动中…") : app.t("Start session", "开始会话")}
            </Button>
          </div>
        }
      />
    );
  }

  const base = ws(`/v1/sessions/${sid}`);
  return (
    <Card style={{ padding: "12px 14px" }}>
      <div className="row" style={{ justifyContent: "space-between", marginBottom: 8 }}>
        <span className="row">
          <Pill tone="agent">{agentId}</Pill>
          {chosen?.config.runtime && <Pill tone="info">⚙ {chosen.config.runtime}{chosen.config.sandbox ? " · sandboxed" : ""}</Pill>}
          <code className="mut" style={{ fontSize: 11 }}>
            {sid}
          </code>
          <UsageBadges usage={session.data?.usage} latencyMs={latencyMs} />
        </span>
        <Button variant="ghost" style={{ height: 24 }} onClick={() => start.mutate()}>
          ↻ {app.t("New session", "新会话")}
        </Button>
      </div>
      {dirty && (
        <div className="banner warn" style={{ marginBottom: 8 }}>
          <span>⚠</span>
          <span>
            {app.t(
              "Unsaved edits aren't live yet — Save + Publish to test them.",
              "未保存的修改尚未生效——保存并发布后再测试。",
            )}
          </span>
        </div>
      )}
      <Transcript
        base={base}
        queryKey={["sandbox-events", sid]}
        placeholder={app.t("Ask the agent…", "问问这个 agent…")}
        onLatency={setLatencyMs}
      />
    </Card>
  );
}
