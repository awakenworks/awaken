// Session detail: header actions (rename/archive/pause/resume/interrupt) + an
// agent/properties aside around the shared <Transcript>. The transcript owns the
// live event log; header actions post inbound events and invalidate the same
// events cache key so the transcript refreshes.

import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Link, useParams } from "react-router";
import Transcript from "../components/session/Transcript";
import { Button, Card, Pill } from "../components/ui";
import { api, ws } from "../lib/api/client";
import type { InboundEvent, Session } from "../lib/api/types";
import { useApp } from "../lib/app-state";

export default function SessionDetailSurface() {
  const app = useApp();
  const { ws: wsId = "default", sid = "" } = useParams();
  const qc = useQueryClient();
  // Workspace-scoped via ws() (tenancy is an edge aspect); flat under default scope.
  const base = ws(`/v1/sessions/${sid}`);
  const eventsKey = ["session-events", wsId, sid];

  const session = useQuery({
    queryKey: ["session", wsId, sid],
    queryFn: () => api.get<Session>(base),
    refetchInterval: 15_000,
  });

  const control = useMutation({
    mutationFn: (evs: InboundEvent[]) => api.post(`${base}/events`, { events: evs }),
    onSuccess: () => {
      void qc.invalidateQueries({ queryKey: eventsKey });
      void session.refetch();
    },
  });
  const rename = useMutation({
    mutationFn: (title: string) => api.post<Session>(base, { title }),
    onSuccess: (s) => {
      qc.setQueryData(["session", wsId, sid], s);
      void qc.invalidateQueries({ queryKey: ["sessions", wsId] });
    },
  });
  const archive = useMutation({
    mutationFn: () => api.post<Session>(`${base}/archive`),
    onSuccess: (s) => {
      qc.setQueryData(["session", wsId, sid], s);
      void qc.invalidateQueries({ queryKey: ["sessions", wsId] });
    },
  });

  return (
    <>
      <div className="row" style={{ justifyContent: "space-between" }}>
        <span>
          <Link to={`/w/${wsId}/sessions`}>‹ Sessions</Link>{" "}
          <code style={{ marginLeft: 8 }}>{sid}</code>{" "}
          {session.data?.title && <strong style={{ marginLeft: 6 }}>{session.data.title}</strong>}
          {session.data?.archived_at && (
            <Pill tone="neutral" style={{ marginLeft: 8 }}>
              {app.t("archived", "已归档")}
            </Pill>
          )}
        </span>
        <span className="row">
          <Button
            variant="ghost"
            onClick={() => {
              const next = prompt(app.t("Session title", "会话标题"), session.data?.title ?? "");
              if (next !== null) rename.mutate(next);
            }}
          >
            ✎ {app.t("rename", "重命名")}
          </Button>
          {!session.data?.archived_at && (
            <Button variant="ghost" onClick={() => archive.mutate()}>
              ⌫ {app.t("archive", "归档")}
            </Button>
          )}
          <Button variant="ghost" onClick={() => control.mutate([{ type: "user.pause" }])}>
            ⏸ pause
          </Button>
          <Button variant="ghost" onClick={() => control.mutate([{ type: "user.resume" }])}>
            ▶ resume
          </Button>
          <Button variant="danger" onClick={() => control.mutate([{ type: "user.interrupt" }])}>
            ⏹ interrupt
          </Button>
        </span>
      </div>

      <div style={{ display: "flex", gap: 14, alignItems: "flex-start" }}>
        <div style={{ flex: 1.8, minWidth: 0 }}>
          {session.error instanceof Error && <div className="err">{session.error.message}</div>}
          <Transcript base={base} queryKey={eventsKey} modelOverride />
        </div>

        <aside style={{ width: 300, flex: "none", display: "flex", flexDirection: "column", gap: 12 }}>
          <Card style={{ padding: "12px 14px" }}>
            <h2 style={{ fontSize: 12, textTransform: "uppercase", letterSpacing: ".06em", color: "var(--fg3)" }}>
              Agent
            </h2>
            {session.data ? (
              <>
                <div className="row">
                  <Pill tone="agent">{session.data.agent.id}</Pill>
                  {session.data.agent.model && <code>{session.data.agent.model}</code>}
                </div>
                <div className="mut" style={{ marginTop: 8, fontSize: 12 }}>
                  tools {session.data.agent.tools?.length ?? 0} · skills {session.data.agent.skills?.length ?? 0} · mcp{" "}
                  {session.data.agent.mcp_servers?.length ?? 0}
                </div>
              </>
            ) : (
              <span className="mut">…</span>
            )}
          </Card>
          <Card style={{ padding: "12px 14px" }}>
            <h2 style={{ fontSize: 12, textTransform: "uppercase", letterSpacing: ".06em", color: "var(--fg3)" }}>
              {app.t("Properties", "属性")}
            </h2>
            <div className="mut" style={{ fontSize: 12, display: "flex", flexDirection: "column", gap: 4 }}>
              <span>created {session.data?.created_at ?? "—"}</span>
              <span>status {session.data?.status ?? "—"}</span>
              <span>env {session.data?.environment_id ?? "—"}</span>
            </div>
          </Card>
          <Card style={{ padding: "12px 14px" }}>
            <h2 style={{ fontSize: 12, textTransform: "uppercase", letterSpacing: ".06em", color: "var(--fg3)" }}>
              Durable ops
            </h2>
            <span className="mut" style={{ fontSize: 12 }}>
              {app.t("Enabled only under AWAKEN_INGRESS=durable.", "仅在 AWAKEN_INGRESS=durable 下可用。")}
            </span>
          </Card>
        </aside>
      </div>
    </>
  );
}
