// Session detail: header actions (rename/archive/pause/resume/interrupt) + an
// agent/properties aside around the shared <Transcript>. The transcript owns the
// live event log; header actions post inbound events and invalidate the same
// events cache key so the transcript refreshes.

import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useState } from "react";
import { Link, useParams } from "react-router";
import Transcript from "../components/session/Transcript";
import TraceView from "../components/session/TraceView";
import SessionFiles from "../components/session/SessionFiles";
import { Button, Card, Pill, Segmented } from "../components/ui";
import { api, ws } from "../lib/api/client";
import type { InboundEvent, Session } from "../lib/api/types";
import { useApp } from "../lib/app-state";

/** The agent's model can arrive as a bare id or a `{ id }` object — coerce to text. */
function modelText(m: unknown): string {
  if (typeof m === "string") return m;
  if (m && typeof m === "object" && "id" in m) return String((m as { id: unknown }).id);
  return "";
}

export default function SessionDetailSurface() {
  const app = useApp();
  const { ws: wsId = "default", sid = "" } = useParams();
  const qc = useQueryClient();
  // Workspace-scoped via ws() (tenancy is an edge aspect); flat under default scope.
  const base = ws(`/v1/sessions/${sid}`);
  const eventsKey = ["session-events", wsId, sid];
  const [view, setView] = useState<"chat" | "trace" | "files">("chat");

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
          <Segmented
            className="segmented"
            options={[
              { value: "chat", label: app.t("Chat", "对话") },
              { value: "trace", label: app.t("Trace", "追踪") },
              { value: "files", label: app.t("Files", "文件") },
            ]}
            value={view}
            onChange={setView}
          />
          {view === "chat" && <Transcript base={base} queryKey={eventsKey} modelOverride />}
          {view === "trace" && <TraceView base={base} queryKey={eventsKey} />}
          {view === "files" && <SessionFiles base={base} sid={sid} />}
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
                  {modelText(session.data.agent.model) && <code>{modelText(session.data.agent.model)}</code>}
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
              {/* Runtime provenance: which backend actually executed this run (native vs an
                  ACP CLI), read off the session metadata the environment stamped at create. */}
              <span className="row" style={{ gap: 6, alignItems: "center" }}>
                runtime
                <Pill tone={session.data?.metadata?.["awaken.runtime"] ? "agent" : "neutral"}>
                  {session.data?.metadata?.["awaken.runtime"] ?? "native"}
                </Pill>
              </span>
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
