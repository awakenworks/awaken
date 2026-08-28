// Session detail: header actions (rename/archive/interrupt) + an
// agent/properties aside around the shared TranscriptView. This surface owns
// exactly one live SessionLog; header, chat, approvals, and trace consume it.

import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { managedSessionPresentationPhase } from "@awaken/managed-session-projection";
import { useState } from "react";
import { Link, useParams } from "react-router";
import { TranscriptView } from "../components/session/Transcript";
import TraceView from "../components/session/TraceView";
import SessionFiles from "../components/session/SessionFiles";
import SessionIntegrations from "../components/session/SessionIntegrations";
import SessionThreads from "../components/session/SessionThreads";
import { Button, Card, Modal, Pill, Segmented, TextField, useConfirm, useToast } from "../components/ui";
import { api, ws } from "../lib/api/client";
import type { Session } from "../lib/api/types";
import { useApp } from "../lib/app-state";
import { sessionErrorText } from "../lib/session-log";
import { useSessionLog } from "../lib/useSessionLog";

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
  // One hook owns merge/reducer/admission, SSE, mutation identity, and pending
  // transport state for every detail control and view.
  const sessionLog = useSessionLog(base, eventsKey, {
    sessionStatus: session.data?.status,
    live: true,
    followLive: true,
  });
  const runtime = sessionLog.runtime;
  const admission = sessionLog.admission;
  const effectiveStatus = managedSessionPresentationPhase(runtime, session.data?.status);
  const needsRecovery = runtime.pendingToolIds.size > 0
    || runtime.resolvingToolIds.size > 0
    || runtime.resolvingInputIds.size > 0;

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
      title: needsRecovery
        ? app.t("Recover this stuck run?", "恢复这个卡住的运行？")
        : app.t("Stop the current run?", "停止当前运行？"),
      body: needsRecovery
        ? app.t("The committed pending tool requests are interrupted through the Session event stream. Conversation, repository changes, and history remain available, and a new message can be sent after the terminal event is projected.", "将通过 Session 事件流中断已提交的待处理工具请求；对话、仓库修改和历史都会保留，终止事件投影后即可发送新消息。")
        : app.t("The current model turn is interrupted. The conversation and completed work remain available, and you can send a new message afterwards.", "当前模型回合会被中断；对话与已完成工作仍会保留，之后可以继续发送新消息。"),
      confirmLabel: needsRecovery
        ? app.t("Recover run", "恢复运行")
        : app.t("Stop run", "停止运行"),
      danger: true,
    });
    if (!approved) return;
    setControlResult(null);
    try {
      const result = await sessionLog.send([{ type: "user.interrupt" }]);
      const receipt = result.data?.at(-1);
      setControlResult(receipt ? `${receipt.type} accepted · ${receipt.id}` : null);
      void session.refetch();
    } catch {
      // The shared hook owns and presents the exact transport/projection error.
    }
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
          <Button variant="danger" disabled={!admission.canInterrupt || sessionLog.sendPending} onClick={() => void interruptSession()}>
            ⏹ {needsRecovery ? app.t("Recover run", "恢复运行") : app.t("Stop run", "停止运行")}
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

      {controlResult && <div className="banner info">{controlResult}</div>}
      {sessionLog.sendError && <div className="banner err">{sessionLog.sendError.message}</div>}
      {view !== "chat" && sessionLog.projectionError && (
        <div className="banner err">
          {app.t(
            "Committed Session history is inconsistent; input is disabled until the projection is reloaded.",
            "已提交的会话历史不一致；重新加载投影前将禁用输入。",
          )}
        </div>
      )}

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
          {view === "chat" && <TranscriptView key={sid} sessionLog={sessionLog} />}
          {view === "collaboration" && <SessionThreads base={base} workspaceId={wsId} />}
          {view === "inputs" && <SessionFiles base={base} sid={sid} view="inputs" />}
          {view === "artifacts" && <SessionFiles base={base} sid={sid} view="artifacts" />}
          {view === "integrations" && <SessionIntegrations session={session.data} />}
          {view === "trace" && (
            <TraceView
              log={sessionLog.log}
              loadError={sessionLog.projectionError ? null : sessionLog.loadError}
            />
          )}
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
              <span>{app.t("Status", "状态")} {effectiveStatus === "running" ? app.t("running", "运行中") : effectiveStatus === "idle" ? app.t("idle", "空闲") : effectiveStatus ?? "—"}</span>
              <span>{app.t("Pending tools", "待审批工具")} {runtime.pendingToolIds.size}</span>
              <span>{app.t("Resolving tools", "处理中工具")} {runtime.resolvingToolIds.size}</span>
              <span>{app.t("Resolving inputs", "处理中输入")} {runtime.resolvingInputIds.size}</span>
              <span>{app.t("Can send message", "允许发送消息")} {admission.canSendMessage ? app.t("yes", "是") : app.t("no", "否")}</span>
              {runtime.latestError && <span className="err">{app.t("Last error", "最近错误")} {sessionErrorText(runtime.latestError)}</span>}
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
