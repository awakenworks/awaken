import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useMemo, useState } from "react";
import { Link } from "react-router";
import type { ListEventsResponse, Page, SessionEvent, SessionThread } from "../../lib/api/types";
import { api } from "../../lib/api/client";
import { useApp } from "../../lib/app-state";
import { Button, Card, EmptyState, Pill, useConfirm, useToast } from "../ui";

function usageTotal(thread: SessionThread): number {
  return (thread.usage?.input_tokens ?? 0)
    + (thread.usage?.output_tokens ?? 0)
    + (thread.usage?.cache_read_input_tokens ?? 0)
    + (thread.usage?.cache_creation?.ephemeral_1h_input_tokens ?? 0)
    + (thread.usage?.cache_creation?.ephemeral_5m_input_tokens ?? 0);
}

function eventText(event: SessionEvent): string {
  if (event.type === "agent.message" && "content" in event && Array.isArray(event.content)) {
    const text = event.content
      .filter((block) => block.type === "text" && typeof block.text === "string")
      .map((block) => block.text)
      .join("\n");
    if (text) return text;
  }
  if (event.type === "session.error") {
    if ("error" in event && event.error && typeof event.error === "object" && "message" in event.error) {
      return String(event.error.message ?? event.type);
    }
    if ("message" in event && typeof event.message === "string") return event.message;
  }
  return event.type;
}

function statusTone(status: SessionThread["status"]): "ok" | "warn" | "danger" | "agent" {
  if (status === "idle") return "ok";
  if (status === "terminated") return "danger";
  if (status === "rescheduling") return "warn";
  return "agent";
}

export default function SessionThreads({ base, workspaceId }: { base: string; workspaceId: string }) {
  const app = useApp();
  const qc = useQueryClient();
  const confirm = useConfirm();
  const toast = useToast();
  const [selectedId, setSelectedId] = useState("");
  const threadsKey = ["session-threads", workspaceId, base];
  const threads = useQuery({
    queryKey: threadsKey,
    queryFn: () => api.get<Page<SessionThread>>(`${base}/threads`),
    refetchInterval: (query) => query.state.data?.data.some((thread) => thread.status === "running") ? 3_000 : false,
  });
  const primary = threads.data?.data.find((thread) => thread.parent_thread_id == null);
  const children = threads.data?.data.filter((thread) => thread.parent_thread_id != null) ?? [];
  const selected = useMemo(
    () => threads.data?.data.find((thread) => thread.id === selectedId),
    [selectedId, threads.data?.data],
  );
  const events = useQuery({
    queryKey: ["session-thread-events", workspaceId, base, selectedId],
    enabled: Boolean(selectedId),
    queryFn: () => api.get<ListEventsResponse>(`${base}/threads/${encodeURIComponent(selectedId)}/events?limit=100`),
    refetchInterval: selected?.status === "running" ? 3_000 : false,
  });
  const stopThread = useMutation({
    mutationFn: (threadId: string) => api.post<SessionThread>(`${base}/threads/${encodeURIComponent(threadId)}/archive`),
    onSuccess: (thread) => {
      toast.ok(app.t(`Stopped ${thread.agent.name || thread.agent.id}.`, `已停止 ${thread.agent.name || thread.agent.id}。`));
      void qc.invalidateQueries({ queryKey: threadsKey });
    },
    onError: (error) => toast.err(error instanceof Error ? error.message : String(error)),
  });
  const stop = async (thread: SessionThread) => {
    const approved = await confirm({
      title: app.t("Stop this child run?", "停止此子运行？"),
      body: app.t(
        "Completed output remains available. This does not stop the parent Session or retry the delegated task.",
        "已完成的输出仍会保留；此操作不会停止父会话，也不会重试已委托任务。",
      ),
      confirmLabel: app.t("Stop child run", "停止子运行"),
      danger: true,
    });
    if (approved) stopThread.mutate(thread.id);
  };

  if (threads.error instanceof Error) {
    return <Card><EmptyState title={app.t("Collaboration runs could not load", "无法加载协作运行")} hint={threads.error.message} action={<Button onClick={() => void threads.refetch()}>{app.t("Retry", "重试")}</Button>} /></Card>;
  }

  return (
    <div className="session-threads">
      <Card className="session-thread-summary">
        <div>
          <h2>{app.t("Collaboration runs", "协作运行")}</h2>
          <p className="hint">{app.t(
            "The parent and every delegated child run are projections of this Session's durable event history.",
            "父运行和每个委托子运行都是当前会话持久事件历史的投影。",
          )}</p>
        </div>
        <div className="row" style={{ flexWrap: "wrap" }}>
          <Pill tone="neutral">{app.t("Parent", "父运行")} {primary ? 1 : 0}</Pill>
          <Pill tone={children.some((thread) => thread.status === "running") ? "agent" : "neutral"}>
            {app.t("Child runs", "子运行")} {children.length}
          </Pill>
          <Pill tone="neutral">{app.t("Total tokens", "Token 总计")} {(threads.data?.data ?? []).reduce((sum, thread) => sum + usageTotal(thread), 0)}</Pill>
        </div>
      </Card>

      {threads.isLoading ? (
        <Card><span className="mut">{app.t("Loading collaboration runs…", "正在加载协作运行…")}</span></Card>
      ) : children.length === 0 ? (
        <Card>
          <EmptyState
            title={app.t("No child runs in this Session", "此会话中没有子运行")}
            hint={app.t(
              "The parent Agent has not delegated work yet. Child runs appear here as soon as delegation starts.",
              "父 Agent 尚未委托任务；委托开始后，子运行会立即显示在这里。",
            )}
          />
          {primary && (
            <div className="primary-thread-row">
              <Pill tone={statusTone(primary.status)}>{primary.status}</Pill>
              <strong>{primary.agent.name || primary.agent.id}</strong>
              <span className="mono mut">{primary.id}</span>
            </div>
          )}
        </Card>
      ) : (
        <div className="thread-tree">
          {primary && (
            <div className="thread-node thread-node-primary">
              <div>
                <span className="thread-node-kind">{app.t("Parent", "父运行")}</span>
                <strong>{primary.agent.name || primary.agent.id}</strong>
                <span className="mono mut">{primary.id}</span>
              </div>
              <Pill tone={statusTone(primary.status)}>{primary.status}</Pill>
            </div>
          )}
          <div className="thread-children">
            {children.map((thread) => (
              <Card className="thread-node thread-node-child" key={thread.id}>
                <div className="thread-node-main">
                  <span className="thread-tree-branch" aria-hidden="true">↳</span>
                  <div>
                    <span className="thread-node-kind">{app.t("Auxiliary Agent", "辅助 Agent")}</span>
                    <strong>{thread.agent.name || thread.agent.id}</strong>
                    <Link className="mono mut" to={`/w/${workspaceId}/agents/${thread.agent.id}`}>{thread.agent.id} · v{thread.agent.version}</Link>
                  </div>
                </div>
                <div className="thread-node-metrics">
                  <Pill tone={statusTone(thread.status)}>{thread.status}</Pill>
                  <span>{app.t("Tokens", "Token")} {usageTotal(thread) || "—"}</span>
                  <span>{app.t("Duration", "耗时")} {thread.stats?.duration_seconds !== undefined ? `${thread.stats.duration_seconds.toFixed(1)}s` : "—"}</span>
                  <span>{app.t("Updated", "更新时间")} {new Date(thread.updated_at).toLocaleTimeString()}</span>
                </div>
                <div className="row thread-node-actions">
                  <Button variant={selectedId === thread.id ? "primary" : "ghost"} onClick={() => setSelectedId(thread.id)}>
                    {app.t("Inspect events", "查看事件")}
                  </Button>
                  {thread.status !== "terminated" && (
                    <Button variant="danger" disabled={stopThread.isPending} onClick={() => void stop(thread)}>
                      {app.t("Stop", "停止")}
                    </Button>
                  )}
                </div>
              </Card>
            ))}
          </div>
        </div>
      )}

      {selected && (
        <Card className="thread-event-panel">
          <div className="row" style={{ justifyContent: "space-between" }}>
            <div>
              <h2>{selected.agent.name || selected.agent.id}</h2>
              <span className="mono mut">{selected.id}</span>
            </div>
            <Button variant="ghost" onClick={() => setSelectedId("")}>{app.t("Close", "关闭")}</Button>
          </div>
          {events.error instanceof Error && <div className="err">{events.error.message}</div>}
          {events.isLoading ? <span className="mut">{app.t("Loading events…", "正在加载事件…")}</span> : (
            <div className="thread-events" role="log" aria-label={app.t("Child run events", "子运行事件")}>
              {(events.data?.data ?? []).length === 0 ? (
                <span className="mut">{app.t("No projected events yet.", "暂无投影事件。")}</span>
              ) : (events.data?.data ?? []).map((event) => (
                <div className="thread-event" key={event.id}>
                  <Pill tone={event.type === "session.error" ? "danger" : "neutral"}>{event.type}</Pill>
                  <span>{eventText(event)}</span>
                  <small className="mono mut">{event.id}</small>
                </div>
              ))}
            </div>
          )}
          <p className="hint">{app.t(
            "A child run cannot be retried safely without a new delegation request from its parent, so this view does not invent a retry action.",
            "没有父 Agent 发起新的委托请求，子运行无法安全重试，因此这里不会提供虚假的重试操作。",
          )}</p>
        </Card>
      )}
    </div>
  );
}
