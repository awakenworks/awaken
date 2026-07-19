// The session transcript: a pure projection over the committed event log, with
// inline HITL (approve/deny a gated tool) and a composer. Self-contained — it
// owns the live log via useSessionLog — so the same engine backs the session
// detail, the editor Sandbox, the Admin Assistant, and the model Test modal.

import { useEffect, useRef, useState, type ReactNode } from "react";
import { Button, Card, Pill } from "../ui";
import type { ContentBlock, InboundEvent, SessionEvent } from "../../lib/api/types";
import { useApp } from "../../lib/app-state";
import { textOf } from "../../lib/session-log";
import { useSessionLog } from "../../lib/useSessionLog";

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
            <Button onClick={() => onConfirm(false, note)}>{app.t("Deny", "拒绝")}</Button>
            <Button variant="primary" onClick={() => onConfirm(true, note)}>
              ✓ {app.t("Allow", "允许")}
            </Button>
          </div>
        </div>
      )}
    </details>
  );
}

export interface TranscriptProps {
  /** Session base path, already workspace-scoped via ws(). */
  base: string;
  /** Stable cache key for this session's events. */
  queryKey: readonly unknown[];
  /** Show the message composer (default true). */
  composer?: boolean;
  /** Show the per-message model-override field (default false). */
  modelOverride?: boolean;
  /** Pin every user message to this model (Test-a-model; hides the override field). */
  fixedModel?: string;
  /** Placeholder for the composer input. */
  placeholder?: string;
  /** Rendered above the log (e.g. an empty-state hint). */
  header?: ReactNode;
  /** Poll + subscribe to SSE (default true). */
  live?: boolean;
  /** Round-trip latency (send → next agent message), measured client-side. */
  onLatency?: (ms: number) => void;
  /** Prepended (invisibly to the reader) to each sent message — used to inject
   * task context, e.g. "refine agent X via admin_patch_agent". */
  contextPrefix?: string;
}

export default function Transcript({
  base,
  queryKey,
  composer = true,
  modelOverride = false,
  placeholder,
  header,
  live = true,
  onLatency,
  fixedModel,
  contextPrefix,
}: TranscriptProps) {
  const app = useApp();
  const { log, results, pendingIds, running, freshCount, applyPending, send, sendError, loadError } =
    useSessionLog(base, queryKey, live, composer);
  const [draft, setDraft] = useState("");
  const [model, setModel] = useState("");
  const bottomRef = useRef<HTMLDivElement>(null);
  // Latency: stamp on send, resolve when the next agent.message lands in the log.
  const sentAt = useRef<number | null>(null);
  const agentMsgCount = log.filter((e) => e.type === "agent.message").length;
  const prevMsgCount = useRef(agentMsgCount);
  useEffect(() => {
    if (sentAt.current != null && agentMsgCount > prevMsgCount.current) {
      onLatency?.(Date.now() - sentAt.current);
      sentAt.current = null;
    }
    prevMsgCount.current = agentMsgCount;
  }, [agentMsgCount, onLatency]);

  const confirm = (ev: SessionEvent, allow: boolean, note: string) => {
    const inbound: InboundEvent =
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
          };
    send([inbound]);
  };

  const submit = () => {
    if (!draft.trim()) return;
    const useModel = fixedModel || model;
    sentAt.current = Date.now();
    // A short context tag is prepended to steer the model (e.g. "refine agent X"); it
    // reads as context in the echoed user bubble.
    const text = contextPrefix ? `${contextPrefix}\n${draft.trim()}` : draft.trim();
    send([
      {
        type: "user.message",
        content: [{ type: "text", text }],
        ...(useModel ? { model: useModel } : {}),
      },
    ]);
    setDraft("");
  };

  return (
    <div className="transcript" style={{ display: "flex", flexDirection: "column", gap: 8 }}>
      {header}
      {freshCount > 0 && (
        <Button style={{ alignSelf: "flex-start", borderRadius: 999 }} onClick={applyPending}>
          <span className="dot pulse" style={{ background: "var(--agent)" }} />
          {freshCount} {app.t("new updates · Refresh", "条新事件 · 刷新")}
        </Button>
      )}
      {loadError && <div className="err">{loadError.message}</div>}
      {log.map((ev) => {
        switch (ev.type) {
          case "user.message":
            return (
              <Card
                key={ev.id}
                style={{ padding: "8px 12px", maxWidth: "88%", alignSelf: "flex-end", background: "var(--soft)" }}
              >
                <div style={{ whiteSpace: "pre-wrap", lineHeight: 1.5 }}>
                  {textOf("content" in ev ? (ev.content as ContentBlock[]) : undefined)}
                </div>
              </Card>
            );
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
                result={results.get(ev.id)}
                pendingConfirm={pendingIds.has(ev.id) && !results.get(ev.id)}
                onConfirm={(allow, note) => confirm(ev, allow, note)}
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
              <div
                key={ev.id}
                className={`banner ${sr.type === "retries_exhausted" ? "warn" : "gate"}`}
              >
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
      {composer && (
        <div className="row" style={{ marginTop: 6 }}>
          <input
            className="input"
            style={{ flex: 1, height: 38 }}
            placeholder={placeholder ?? app.t("Message…", "输入消息…")}
            value={draft}
            onChange={(e) => setDraft(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Enter") submit();
            }}
          />
          {modelOverride && (
            <input
              className="input mono"
              style={{ width: 170 }}
              placeholder={app.t("model override", "覆盖模型")}
              value={model}
              onChange={(e) => setModel(e.target.value)}
            />
          )}
        </div>
      )}
      {sendError && <div className="err">{sendError.message}</div>}
    </div>
  );
}
