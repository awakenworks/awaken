// Session detail: header actions (rename/archive/interrupt) + an
// agent/properties aside around the shared <Transcript>. The transcript owns the
// live event log; header actions post inbound events and invalidate the same
// events cache key so the transcript refreshes.

import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useState } from "react";
import { Link, useParams } from "react-router";
import Transcript from "../components/session/Transcript";
import TraceView from "../components/session/TraceView";
import SessionFiles from "../components/session/SessionFiles";
import SessionIntegrations from "../components/session/SessionIntegrations";
import SessionThreads from "../components/session/SessionThreads";
import { Button, Card, Modal, Pill, Segmented, TextField, useConfirm, useToast } from "../components/ui";
import { api, ws } from "../lib/api/client";
import type { InboundEvent, SendEventsResponse, Session } from "../lib/api/types";
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
  const confirm = useConfirm();
  const toast = useToast();
  // Workspace-scoped via ws() (tenancy is an edge aspect); flat under default scope.
  const base = ws(`/v1/sessions/${sid}`);
  const eventsKey = ["session-events", wsId, sid];
  const [view, setView] = useState<"chat" | "collaboration" | "inputs" | "artifacts" | "integrations" | "trace">("chat");
  const [controlResult, setControlResult] = useState<string | null>(null);
  const [renaming, setRenaming] = useState(false);
  const [titleDraft, setTitleDraft] = useState("");

  const session = useQuery({
    queryKey: ["session", wsId, sid],
    queryFn: () => api.get<Session>(base),
    refetchInterval: 15_000,
  });

  const control = useMutation({
    mutationFn: (evs: InboundEvent[]) => api.post<SendEventsResponse>(`${base}/events`, { events: evs }),
    onSuccess: (result) => {
      const receipt = result.data.at(-1);
      setControlResult(receipt ? `${receipt.type} accepted · ${receipt.id}` : null);
      void qc.invalidateQueries({ queryKey: eventsKey });
      void session.refetch();
    },
    onError: (error) => setControlResult(error instanceof Error ? error.message : "control request failed"),
  });
  const rename = useMutation({
    mutationFn: (title: string) => api.post<Session>(base, { title }),
    onSuccess: (s) => {
      qc.setQueryData(["session", wsId, sid], s);
      void qc.invalidateQueries({ queryKey: ["sessions", wsId] });
      setRenaming(false);
      toast.ok(app.t("Session renamed.", "会话已重命名。"));
    },
    onError: (cause) => toast.err(cause instanceof Error ? cause.message : String(cause)),
  });
  const archive = useMutation({
    mutationFn: () => api.post<Session>(`${base}/archive`),
    onSuccess: (s) => {
      qc.setQueryData(["session", wsId, sid], s);
      void qc.invalidateQueries({ queryKey: ["sessions", wsId] });
      toast.ok(app.t("Session archived.", "会话已归档。"));
    },
    onError: (cause) => toast.err(cause instanceof Error ? cause.message : String(cause)),
  });
  const archiveSession = async () => {
    const approved = await confirm({
      title: app.t("Archive this session?", "归档该会话？"),
      body: app.t("It remains readable, but no new work should be sent to it.", "它仍可读取，但不应再向其发送新任务。"),
      confirmLabel: app.t("Archive", "归档"),
    });
    if (approved) archive.mutate();
  };
  const interruptSession = async () => {
    const approved = await confirm({
      title: app.t("Stop the current run?", "停止当前运行？"),
      body: app.t("The current model turn is interrupted. The conversation and completed work remain available, and you can send a new message afterwards.", "当前模型回合会被中断；对话与已完成工作仍会保留，之后可以继续发送新消息。"),
      confirmLabel: app.t("Stop run", "停止运行"),
      danger: true,
    });
    if (approved) control.mutate([{ type: "user.interrupt" }]);
  };

  return (
    <>
      <div className="row" style={{ justifyContent: "space-between" }}>
        <span>
          <Link to={`/w/${wsId}/sessions`}>‹ {app.t("Sessions", "会话")}</Link>{" "}
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
              setTitleDraft(session.data?.title ?? "");
              setRenaming(true);
            }}
          >
            ✎ {app.t("Rename", "重命名")}
          </Button>
          {!session.data?.archived_at && (
            <Button variant="ghost" disabled={archive.isPending} onClick={() => void archiveSession()}>
              ⌫ {archive.isPending ? app.t("Archiving…", "正在归档…") : app.t("Archive", "归档")}
            </Button>
          )}
          <Button variant="danger" disabled={session.data?.status !== "running" || control.isPending} onClick={() => void interruptSession()}>
            ⏹ {app.t("Stop run", "停止运行")}
          </Button>
        </span>
      </div>

      {renaming && (
        <Modal title={app.t("Rename session", "重命名会话")} onClose={() => setRenaming(false)}>
          <TextField label={app.t("Title", "标题")} value={titleDraft} onChange={(event) => setTitleDraft(event.target.value)} />
          {rename.error instanceof Error && <div className="err">{rename.error.message}</div>}
          <div className="row" style={{ justifyContent: "flex-end" }}>
            <Button onClick={() => setRenaming(false)}>{app.t("Cancel", "取消")}</Button>
            <Button variant="primary" disabled={rename.isPending} onClick={() => rename.mutate(titleDraft)}>
              {rename.isPending ? app.t("Saving…", "正在保存…") : app.t("Save", "保存")}
            </Button>
          </div>
        </Modal>
      )}

      {controlResult && <div className={`banner ${control.isError ? "err" : "info"}`}>{controlResult}</div>}

      <div className="session-detail-layout" style={{ display: "flex", gap: 14, alignItems: "flex-start" }}>
        <div style={{ flex: 1.8, minWidth: 0 }}>
          {session.error instanceof Error && <div className="err">{session.error.message}</div>}
          <Segmented
            className="segmented"
            options={[
              { value: "chat", label: app.t("Chat", "对话") },
              { value: "collaboration", label: app.t("Child runs", "子运行") },
              { value: "inputs", label: app.t("Inputs", "输入") },
              { value: "artifacts", label: app.t("Artifacts", "产物") },
              { value: "integrations", label: app.t("Integrations", "集成") },
              { value: "trace", label: app.t("Trace", "追踪") },
            ]}
            value={view}
            onChange={setView}
          />
          {view === "chat" && <Transcript base={base} queryKey={eventsKey} modelOverride />}
          {view === "collaboration" && <SessionThreads base={base} workspaceId={wsId} />}
          {view === "inputs" && <SessionFiles base={base} sid={sid} view="inputs" />}
          {view === "artifacts" && <SessionFiles base={base} sid={sid} view="artifacts" />}
          {view === "integrations" && <SessionIntegrations session={session.data} />}
          {view === "trace" && <TraceView base={base} queryKey={eventsKey} />}
        </div>

        <aside className="session-detail-aside" style={{ width: 300, flex: "none", display: "flex", flexDirection: "column", gap: 12 }}>
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
                  {app.t("Tools", "工具")} {session.data.agent.tools?.length ?? 0} · Skills {session.data.agent.skills?.length ?? 0} · MCP {session.data.agent.mcp_servers?.length ?? 0}
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
              <span>{app.t("Created", "创建时间")} {session.data?.created_at ? new Date(session.data.created_at).toLocaleString() : "—"}</span>
              <span>{app.t("Status", "状态")} {session.data?.status === "running" ? app.t("running", "运行中") : session.data?.status === "idle" ? app.t("idle", "空闲") : session.data?.status ?? "—"}</span>
              <span>{app.t("Environment", "运行环境")} {session.data?.environment_id ?? app.t("Default", "默认")}</span>
              {/* Runtime provenance: which backend actually executed this run (native vs an
                  ACP CLI), read off the session metadata the environment stamped at create. */}
              <span className="row" style={{ gap: 6, alignItems: "center" }}>
                {app.t("Runtime", "运行时")}
                <Pill tone={session.data?.metadata?.["awaken.runtime"] ? "agent" : "neutral"}>
                  {session.data?.metadata?.["awaken.runtime"] ?? app.t("Awaken native", "Awaken 原生")}
                </Pill>
              </span>
            </div>
          </Card>
        </aside>
      </div>
    </>
  );
}
