// The session transcript: a pure projection over the committed event log, with
// inline HITL (approve/deny a gated tool) and a composer. Self-contained — it
// owns the live log via useSessionLog — so the same engine backs the session
// detail, the editor Sandbox, the Admin Assistant, and the model Test modal.

import { useEffect, useRef, useState, type ReactNode } from "react";
import { Button, Card, Pill } from "../ui";
import type { ContentBlock, InboundEvent, SessionEvent } from "../../lib/api/types";
import { useApp } from "../../lib/app-state";
import { sessionErrorText, textOf } from "../../lib/session-log";
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
  /** Send a parent-authored message once. Used by editor workflows that hand a
   * validation failure to the Admin Assistant without asking the operator to copy it. */
  autoMessage?: { id: string; text: string };
  /** Reports completed tools without exposing the session-log hook to consumers. */
  onToolComplete?: (tool: SessionEvent, result: SessionEvent) => void;
  /** Fires after a locally started request returns to idle. */
  onRunSettled?: () => void;
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
  autoMessage,
  onToolComplete,
  onRunSettled,
}: TranscriptProps) {
  const app = useApp();
  const { log, results, pendingIds, running, freshCount, applyPending, send, sendPending, sendError, loadError } =
    useSessionLog(base, queryKey, live, composer);
  const [draft, setDraft] = useState("");
  const [pendingMessage, setPendingMessage] = useState<string | null>(null);
  const [model, setModel] = useState("");
  const bottomRef = useRef<HTMLDivElement>(null);
  const composerRef = useRef<HTMLTextAreaElement>(null);
  const handledTools = useRef(new Set<string>());
  const handledAutoMessage = useRef<string | null>(null);
  const hadLocalActivity = useRef(false);
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

  // Echo the operator's first message immediately. The events POST may remain open
  // for the whole run, and the first committed/SSE frame can arrive later; neither
  // should make a freshly submitted chat look unresponsive.
  useEffect(() => {
    if (!pendingMessage) return;
    const committed = log.some((event) =>
      event.type === "user.message" &&
      textOf("content" in event ? (event.content as ContentBlock[]) : undefined).endsWith(pendingMessage));
    if (committed) setPendingMessage(null);
  }, [log, pendingMessage]);

  useEffect(() => {
    bottomRef.current?.scrollIntoView({ block: "nearest" });
  }, [log.length, pendingMessage, running, sendPending]);

  useEffect(() => {
    if (!onToolComplete) return;
    for (const event of log) {
      if (event.type !== "agent.tool_use" && event.type !== "agent.custom_tool_use") continue;
      const result = results.get(event.id);
      if (!result || handledTools.current.has(event.id)) continue;
      handledTools.current.add(event.id);
      onToolComplete(event, result);
    }
  }, [log, onToolComplete, results]);

  const sendText = (userText: string, useModel = fixedModel || model) => {
    sentAt.current = Date.now();
    const text = contextPrefix ? `${contextPrefix}\n${userText}` : userText;
    setPendingMessage(userText);
    hadLocalActivity.current = true;
    send([
      {
        type: "user.message",
        content: [{ type: "text", text }],
        ...(useModel ? { model: useModel } : {}),
      },
    ]);
  };

  useEffect(() => {
    if (!autoMessage || handledAutoMessage.current === autoMessage.id || sendPending || running) return;
    handledAutoMessage.current = autoMessage.id;
    sendText(autoMessage.text);
  // `sendText` intentionally uses the current session/context. An auto-message id is
  // the idempotency boundary; changing render-local callback identities must not resend.
  // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [autoMessage?.id, running, sendPending]);

  useEffect(() => {
    if (running || sendPending) return;
    if (!hadLocalActivity.current) return;
    hadLocalActivity.current = false;
    onRunSettled?.();
  }, [onRunSettled, running, sendPending]);

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
    if (!draft.trim() || sendPending) return;
    // Preserve the operator's exact multiline text (indentation and trailing newline
    // can be meaningful in code/prompts); trimming is only the emptiness check above.
    const userText = draft;
    sendText(userText);
    setDraft("");
    if (composerRef.current) composerRef.current.style.height = "auto";
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
            // Historical running frames are facts for Trace, not permanent chat
            // messages. The derived live indicator below is the only working state.
            return null;
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
          case "session.error":
            return (
              <div key={ev.id} className="banner warn" style={{ alignItems: "flex-start" }}>
                <span>✕</span>
                <span>
                  <strong>{app.t("Run failed", "运行失败")}</strong>
                  <br />
                  {sessionErrorText(ev)}
                </span>
              </div>
            );
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
      {pendingMessage && !log.some((event) => event.type === "user.message" && textOf("content" in event ? (event.content as ContentBlock[]) : undefined).endsWith(pendingMessage)) && (
        <Card className="transcript-pending-message" style={{ padding: "8px 12px", maxWidth: "88%", alignSelf: "flex-end", background: "var(--soft)" }}>
          <div style={{ whiteSpace: "pre-wrap", lineHeight: 1.5 }}>{pendingMessage}</div>
          <span className={sendError ? "err" : "mut"} style={{ fontSize: 10.5 }}>
            {sendError ? app.t("send failed — message retained", "发送失败——消息已保留") : app.t("sending…", "发送中…")}
          </span>
        </Card>
      )}
      {(running || sendPending) && (
        <div className="agent-working row" role="status" aria-live="polite">
          <span className="dot pulse" style={{ background: "var(--agent)" }} />
          <span>{app.t("Agent is working…", "Agent 正在处理…")}</span>
        </div>
      )}
      <div ref={bottomRef} />
      {composer && (
        <div className="transcript-composer">
          <textarea
            ref={composerRef}
            className="input transcript-composer__input"
            rows={1}
            aria-label={app.t("Message to agent", "给 Agent 的消息")}
            placeholder={placeholder ?? app.t("Message…", "输入消息…")}
            value={draft}
            onChange={(e) => {
              setDraft(e.target.value);
              e.currentTarget.style.height = "auto";
              e.currentTarget.style.height = `${Math.min(e.currentTarget.scrollHeight, 180)}px`;
            }}
            onKeyDown={(e) => {
              if (e.key === "Enter" && !e.shiftKey && !e.nativeEvent.isComposing) {
                e.preventDefault();
                submit();
              }
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
          <Button variant="primary" disabled={!draft.trim() || sendPending} onClick={submit}>
            {sendPending ? app.t("Sending…", "发送中…") : app.t("Send", "发送")}
          </Button>
          <div className="transcript-composer__hint">
            {app.t(
              "Enter to send · Shift+Enter for a new line · State the goal; tools and skills handle the details.",
              "Enter 发送 · Shift+Enter 换行 · 只需说明目标，细节交给工具和 Skill。",
            )}
          </div>
        </div>
      )}
      {sendError && <div className="err">{sendError.message}</div>}
    </div>
  );
}
