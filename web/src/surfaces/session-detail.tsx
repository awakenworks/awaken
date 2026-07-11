// Session detail: the transcript is a pure projection over the committed
// event log (single query cache entry, reducer dedupes by event id). SSE is a
// committed-replay stream, so live frames land in a pending buffer surfaced by
// the "N new updates · Refresh" banner instead of tearing the reading position.

import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useEffect, useRef, useState } from "react";
import { Link, useParams } from "react-router";
import { Button, Card, Pill } from "../components/ui";
import { api, streamUrl, ws } from "../lib/api/client";
import type {
  ContentBlock,
  InboundEvent,
  ListEventsResponse,
  Session,
  SessionEvent,
} from "../lib/api/types";
import { useApp } from "../lib/app-state";

/** Merge new events into the log: dedupe by id, keep arrival order. */
export function mergeEvents(log: SessionEvent[], incoming: SessionEvent[]): SessionEvent[] {
  const seen = new Set(log.map((e) => e.id));
  const fresh = incoming.filter((e) => !seen.has(e.id));
  return fresh.length ? [...log, ...fresh] : log;
}

function textOf(content: ContentBlock[] | undefined): string {
  return (content ?? [])
    .map((b) => ("text" in b && typeof b.text === "string" ? b.text : `[${b.type}]`))
    .join("");
}

function ToolCard({
  ev,
  result,
  pendingConfirm,
  onConfirm,
}: {
  ev: SessionEvent;
  result?: SessionEvent;
  pendingConfirm: boolean;
  onConfirm: (allow: boolean, note: string) => void;
}) {
  const app = useApp();
  const [note, setNote] = useState("");
  const custom = ev.type === "agent.custom_tool_use";
  const name = "name" in ev && typeof ev.name === "string" ? ev.name : "?";
  const isError = result && "is_error" in result && result.is_error === true;
  return (
    <details
      style={{
        borderRadius: 9,
        background: "var(--soft)",
        boxShadow: "inset 0 0 0 1px var(--line)",
        padding: "7px 10px",
        margin: "6px 0",
      }}
      open={pendingConfirm}
    >
      <summary style={{ cursor: "pointer", display: "flex", alignItems: "center", gap: 8 }}>
        <span>🛠</span>
        <code>{name}</code>
        {custom && <Pill tone="neutral">client-executed</Pill>}
        {"evaluated_permission" in ev && typeof ev.evaluated_permission === "string" && (
          <Pill tone="neutral">{ev.evaluated_permission}</Pill>
        )}
        <span style={{ marginLeft: "auto" }}>
          {pendingConfirm ? (
            <Pill tone="warn">{app.t("awaiting approval", "待确认")}</Pill>
          ) : result ? (
            <Pill tone={isError ? "danger" : "ok"}>{isError ? "error" : "done ✓"}</Pill>
          ) : (
            <span className="pill agent">
              <span className="dot pulse" style={{ background: "var(--agent)" }} />
              {app.t("running", "运行中")}
            </span>
          )}
        </span>
      </summary>
      <pre className="mono" style={{ margin: "8px 0 0", whiteSpace: "pre-wrap", fontSize: 11.5 }}>
        {JSON.stringify("input" in ev ? ev.input : null, null, 2)}
      </pre>
      {result && (
        <pre
          className="mono"
          style={{
            margin: "8px 0 0",
            whiteSpace: "pre-wrap",
            fontSize: 11.5,
            color: isError ? "var(--danger)" : "var(--fg2)",
          }}
        >
          {textOf("content" in result ? (result.content as ContentBlock[]) : undefined)}
        </pre>
      )}
      {pendingConfirm && (
        <div
          className="banner warn"
          style={{ marginTop: 8, flexDirection: "column", alignItems: "stretch", gap: 8 }}
        >
          <strong>
            ⚠ {app.t("Approve", "批准")} <code>{name}</code> {app.t("execution?", "执行?")}
          </strong>
          <div className="row">
            <input
              className="input"
              style={{ flex: 1 }}
              placeholder={app.t("deny note (optional)", "拒绝说明(可选)")}
              value={note}
              onChange={(e) => setNote(e.target.value)}
            />
            <Button onClick={() => onConfirm(false, note)}>
              {app.t("Deny", "拒绝")}
            </Button>
            <Button variant="primary" onClick={() => onConfirm(true, note)}>
              ✓ {app.t("Allow", "允许")}
            </Button>
          </div>
        </div>
      )}
    </details>
  );
}

export default function SessionDetailSurface() {
  const app = useApp();
  const { ws: wsId = "default", sid = "" } = useParams();
  const qc = useQueryClient();
  // Workspace-scoped via ws() (tenancy is an edge aspect); flat under default scope.
  const base = ws(`/v1/sessions/${sid}`);
  const [pending, setPending] = useState<SessionEvent[]>([]);
  const [draft, setDraft] = useState("");
  const [model, setModel] = useState("");
  const bottomRef = useRef<HTMLDivElement>(null);

  const session = useQuery({
    queryKey: ["session", wsId, sid],
    queryFn: () => api.get<Session>(base),
    refetchInterval: 15_000,
  });
  const events = useQuery({
    queryKey: ["session-events", wsId, sid],
    queryFn: async () => (await api.get<ListEventsResponse>(`${base}/events`)).data,
    refetchInterval: 5_000,
  });
  const log = events.data ?? [];

  // SSE replay → pending buffer → banner (never tears the reading position).
  useEffect(() => {
    const source = new EventSource(streamUrl(`${base}/events/stream`));
    const onAny = (raw: MessageEvent) => {
      try {
        const ev = JSON.parse(raw.data as string) as SessionEvent;
        if (!ev.id) return;
        setPending((buf) => (buf.some((b) => b.id === ev.id) ? buf : [...buf, ev]));
      } catch {
        /* non-JSON frame */
      }
    };
    // The host names SSE events by their `type`; listen to the known family.
    for (const name of [
      "agent.message",
      "agent.tool_use",
      "agent.tool_result",
      "agent.custom_tool_use",
      "session.status_running",
      "session.status_idle",
      "span.outcome_evaluation_start",
      "span.outcome_evaluation_end",
    ]) {
      source.addEventListener(name, onAny);
    }
    source.onerror = () => source.close();
    return () => source.close();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [base]);

  const freshCount = pending.filter((p) => !log.some((e) => e.id === p.id)).length;
  const applyPending = () => {
    qc.setQueryData<SessionEvent[]>(["session-events", wsId, sid], (old) =>
      mergeEvents(old ?? [], pending),
    );
    setPending([]);
    bottomRef.current?.scrollIntoView({ behavior: "smooth" });
  };

  const send = useMutation({
    mutationFn: (evs: InboundEvent[]) => api.post(`${base}/events`, { events: evs }),
    onSuccess: () => {
      void events.refetch();
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

  // Pair tool_use events with their results; find ids awaiting confirmation.
  const resultsFor = new Map<string, SessionEvent>();
  for (const ev of log) {
    if (ev.type === "agent.tool_result" && "tool_use_id" in ev) {
      resultsFor.set(ev.tool_use_id as string, ev);
    }
  }
  const requiresIds = new Set<string>();
  for (const ev of log) {
    if (ev.type === "session.status_idle" && "stop_reason" in ev) {
      const sr = ev.stop_reason as { type: string; event_ids?: string[] };
      if (sr.type === "requires_action") for (const id of sr.event_ids ?? []) requiresIds.add(id);
    }
  }
  const last = log[log.length - 1];
  const running = last?.type === "session.status_running";

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
          <Button variant="ghost" onClick={() => send.mutate([{ type: "user.pause" }])}>
            ⏸ pause
          </Button>
          <Button variant="ghost" onClick={() => send.mutate([{ type: "user.resume" }])}>
            ▶ resume
          </Button>
          <Button variant="danger" onClick={() => send.mutate([{ type: "user.interrupt" }])}>
            ⏹ interrupt
          </Button>
        </span>
      </div>

      <div style={{ display: "flex", gap: 14, alignItems: "flex-start" }}>
        <div style={{ flex: 1.8, minWidth: 0, display: "flex", flexDirection: "column", gap: 8 }}>
          {freshCount > 0 && (
            <Button style={{ alignSelf: "flex-start", borderRadius: 999 }} onClick={applyPending}>
              <span className="dot pulse" style={{ background: "var(--agent)" }} />
              {freshCount} {app.t("new updates · Refresh", "条新事件 · 刷新")}
            </Button>
          )}
          {session.error instanceof Error && <div className="err">{session.error.message}</div>}
          {log.map((ev) => {
            switch (ev.type) {
              case "agent.message":
                return (
                  <Card key={ev.id} style={{ padding: "10px 14px", maxWidth: "92%" }}>
                    <span className="mut" style={{ fontSize: 10.5 }}>
                      ⬡ agent
                    </span>
                    <div style={{ whiteSpace: "pre-wrap", lineHeight: 1.55 }}>
                      {textOf("content" in ev ? (ev.content as ContentBlock[]) : undefined)}
                    </div>
                  </Card>
                );
              case "agent.tool_use":
              case "agent.custom_tool_use":
                return (
                  <ToolCard
                    key={ev.id}
                    ev={ev}
                    result={resultsFor.get(ev.id)}
                    pendingConfirm={requiresIds.has(ev.id) && !resultsFor.get(ev.id)}
                    onConfirm={(allow, note) =>
                      send.mutate([
                        ev.type === "agent.custom_tool_use"
                          ? {
                              type: "user.custom_tool_result",
                              custom_tool_use_id: ev.id,
                              content: [{ type: "text", text: note || (allow ? "ok" : "denied") }],
                              is_error: !allow,
                            }
                          : {
                              type: "user.tool_confirmation",
                              tool_use_id: ev.id,
                              result: allow ? "allow" : "deny",
                              deny_message: allow ? undefined : note || undefined,
                            },
                      ])
                    }
                  />
                );
              case "session.status_running":
                return (
                  <div key={ev.id} className="row mut" style={{ fontSize: 12 }}>
                    <span className="dot pulse" style={{ background: "var(--agent)" }} />
                    {app.t("Agent working", "Agent 工作中")}
                  </div>
                );
              case "session.status_idle": {
                const sr = ("stop_reason" in ev ? ev.stop_reason : { type: "?" }) as { type: string };
                if (sr.type === "end_turn") return null;
                return (
                  <div key={ev.id} className={`banner ${sr.type === "retries_exhausted" ? "warn" : "gate"}`}>
                    <span>{sr.type === "requires_action" ? "⚠" : "✕"}</span>
                    <span className="mono" style={{ fontSize: 11.5 }}>
                      {sr.type}
                    </span>
                  </div>
                );
              }
              case "span.outcome_evaluation_start":
              case "span.outcome_evaluation_end":
                return (
                  <div key={ev.id} className="row mut" style={{ fontSize: 12 }}>
                    ◇ outcome · iteration {"iteration" in ev ? String(ev.iteration) : "?"}
                    {"result" in ev ? ` → ${String(ev.result)}` : ""}
                  </div>
                );
              case "agent.tool_result":
                return null; // folded into its tool card
              default:
                return (
                  <div key={ev.id} className="mut mono" style={{ fontSize: 11 }}>
                    [{ev.type}]
                  </div>
                );
            }
          })}
          {running && (
            <div className="row mut" style={{ fontSize: 12 }}>
              <span className="dot pulse" style={{ background: "var(--agent)" }} />…
            </div>
          )}
          <div ref={bottomRef} />
          <div className="row" style={{ marginTop: 6 }}>
            <input
              className="input"
              style={{ flex: 1, height: 38 }}
              placeholder={app.t("Message…", "输入消息…")}
              value={draft}
              onChange={(e) => setDraft(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === "Enter" && draft.trim()) {
                  send.mutate([
                    {
                      type: "user.message",
                      content: [{ type: "text", text: draft.trim() }],
                      ...(model ? { model } : {}),
                    },
                  ]);
                  setDraft("");
                }
              }}
            />
            <input
              className="input mono"
              style={{ width: 170 }}
              placeholder={app.t("model override", "覆盖模型")}
              value={model}
              onChange={(e) => setModel(e.target.value)}
            />
          </div>
          {send.error instanceof Error && <div className="err">{send.error.message}</div>}
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
