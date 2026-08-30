// Protocol-specific Preview adapters and their bounded event inspector. Both
// adapters use one Session-bound Application Access Token; management cookies
// and service API keys never enter this browser transport.

import type { BaseEvent, HttpAgent, Message as AgUiMessage } from "@ag-ui/client";
import {
  ChatComposer,
  ChatMarkdown,
  ChatMessage,
  ChatMessageList,
  ChatThinking,
  ToolCallCard,
} from "@awaken/ui";
import { useEffect, useState } from "react";
import {
  applicationProtocolFetch,
  type IssuedApplicationAccessToken,
} from "../../lib/api/client";
import { useApp } from "../../lib/app-state";
import { Button, EmptyState } from "../ui";

export interface PreviewAccess {
  token: IssuedApplicationAccessToken;
  threadId: string;
  agentId: string;
}

export type PreviewProtocol = "ai-sdk" | "ag-ui";
export interface PreviewProtocolEvent {
  id: number;
  protocol: PreviewProtocol;
  direction: "request" | "response";
  type: string;
  payload: unknown;
}

function isProtocolEventAnchor(event: PreviewProtocolEvent): boolean {
  return event.direction === "request"
    || event.type.startsWith("HTTP_")
    || event.type === "STREAM_ERROR"
    || /^(?:RUN|STEP|TOOL_CALL|TEXT_MESSAGE)_(?:START|STARTED|END|FINISHED|ERROR)$/.test(event.type);
}

/** Keep the inspector bounded without allowing token-level stream deltas to
 * erase the request and lifecycle frames that explain what the run did. */
export function retainProtocolEvents(
  events: PreviewProtocolEvent[],
  limit: number,
): PreviewProtocolEvent[] {
  if (events.length <= limit) return events;
  const anchors = events.filter(isProtocolEventAnchor).slice(-Math.min(24, limit));
  const anchorIds = new Set(anchors.map((event) => event.id));
  const recent = events
    .filter((event) => !anchorIds.has(event.id))
    .slice(-(limit - anchors.length));
  return [...anchors, ...recent].sort((left, right) => left.id - right.id);
}

export function protocolEventType(payload: unknown): string {
  if (payload && typeof payload === "object" && "type" in payload && typeof payload.type === "string") {
    return payload.type;
  }
  return "data";
}

/** Inspect a cloned SSE response without consuming the stream used by the real
 * protocol client. Frames are emitted only after a complete SSE data record is
 * available, so chunk boundaries cannot manufacture or merge events. */
export async function inspectProtocolSse(
  stream: ReadableStream<Uint8Array> | null,
  onPayload: (payload: unknown) => void,
): Promise<void> {
  if (!stream) return;
  const reader = stream.getReader();
  const decoder = new TextDecoder();
  let buffer = "";
  const consume = (final = false) => {
    buffer += final ? decoder.decode() : "";
    const records = buffer.split(/\r?\n\r?\n/);
    buffer = final ? "" : (records.pop() ?? "");
    for (const record of records) {
      const data = record.split(/\r?\n/)
        .filter((line) => line.startsWith("data:"))
        .map((line) => line.slice(5).trimStart())
        .join("\n");
      if (!data || data === "[DONE]") continue;
      try {
        onPayload(JSON.parse(data));
      } catch {
        onPayload(data);
      }
    }
  };
  while (true) {
    const { value, done } = await reader.read();
    if (done) break;
    buffer += decoder.decode(value, { stream: true });
    consume();
  }
  consume(true);
}

export function agUiMessageText(message: AgUiMessage): string {
  if (!("content" in message) || message.content == null) return "";
  if (typeof message.content === "string") return message.content;
  return (message.content as Array<{ type: string; text?: string }>)
    .filter((part) => part.type === "text")
    .map((part) => part.text ?? "")
    .join("");
}

export function previewToolLabel(name: string, zh: boolean): string {
  return zh ? `工具 ${name}` : `Tool ${name}`;
}

export function ProtocolEventInspector({
  events,
  onClear,
}: {
  events: PreviewProtocolEvent[];
  onClear: () => void;
}) {
  const app = useApp();
  return (
    <section className="agent-preview-events" aria-label={app.t("Protocol events", "协议事件")}>
      <div className="agent-preview-events__header">
        <span>
          <strong>{app.t("Protocol events", "协议事件")}</strong>{" "}
          <span className="mut">{app.t("requests and stream events from this run", "本次运行的请求与流事件")}</span>
        </span>
        <Button variant="ghost" style={{ height: 24 }} disabled={events.length === 0} onClick={onClear}>
          {app.t("Clear", "清空")}
        </Button>
      </div>
      <div className="agent-preview-events__list" role="log" aria-live="polite">
        {events.length === 0 ? (
          <p className="mut">{app.t("Send a task to see this protocol's event sequence.", "发送任务后可查看该协议的事件顺序。")}</p>
        ) : retainProtocolEvents(events, 60).map((event) => (
          <details className="agent-preview-event" key={event.id}>
            <summary>
              <span aria-hidden="true">{event.direction === "request" ? "→" : "←"}</span>{" "}
              <code>{event.type}</code>
            </summary>
            <pre>{typeof event.payload === "string" ? event.payload : JSON.stringify(event.payload, null, 2)}</pre>
          </details>
        ))}
      </div>
    </section>
  );
}

export function AgUiConversation({
  access,
  agentName,
  active,
  onEvent,
}: {
  access: PreviewAccess;
  agentName?: string | null;
  active: boolean;
  onEvent: (direction: PreviewProtocolEvent["direction"], type: string, payload: unknown) => void;
}) {
  const app = useApp();
  const [draft, setDraft] = useState("");
  const [messages, setMessages] = useState<AgUiMessage[]>([]);
  const [busy, setBusy] = useState(false);
  const [historyReady, setHistoryReady] = useState(false);
  const [error, setError] = useState<string>();
  const [agent, setAgent] = useState<HttpAgent>();
  const displayName = agentName?.trim() || app.t("Current draft", "当前草稿");

  useEffect(() => {
    if (!active || agent) return;
    let cancelled = false;
    void import("@ag-ui/client").then(({ HttpAgent: OfficialHttpAgent }) => {
      if (cancelled) return;
      const fetch = (url: string, init: RequestInit) => applicationProtocolFetch(
        access.token.access_token,
        url,
        init,
      );
      setAgent(new OfficialHttpAgent({
        url: `/v1/ag-ui/agents/${encodeURIComponent(access.agentId)}`,
        agentId: access.agentId,
        threadId: access.threadId,
        fetch,
      }));
    }).catch((cause) => {
      if (!cancelled) setError(cause instanceof Error ? cause.message : String(cause));
    });
    return () => { cancelled = true; };
  }, [access, active, agent]);

  useEffect(() => {
    if (!active || !agent) return;
    let cancelled = false;
    setHistoryReady(false);
    void applicationProtocolFetch(
      access.token.access_token,
      `/v1/ag-ui/threads/${encodeURIComponent(access.threadId)}/messages`,
    ).then(async (response) => {
      if (!response.ok) throw new Error(await response.text());
      const body = await response.json() as { items?: AgUiMessage[] };
      if (cancelled) return;
      agent.messages = body.items ?? [];
      setMessages([...agent.messages]);
      setHistoryReady(true);
    }).catch((cause) => {
      if (!cancelled) setError(cause instanceof Error ? cause.message : String(cause));
    });
    return () => { cancelled = true; };
  }, [access, active, agent]);

  const submit = async () => {
    const text = draft.trim();
    if (!text || busy || !agent || !historyReady) return;
    setDraft("");
    setError(undefined);
    const user: AgUiMessage = { id: `user-${crypto.randomUUID()}`, role: "user", content: text };
    agent.addMessage(user);
    setMessages([...agent.messages]);
    setBusy(true);
    const runId = `run-${crypto.randomUUID()}`;
    onEvent("request", "RUN_AGENT", {
      endpoint: `/v1/ag-ui/agents/${access.agentId}`,
      threadId: access.threadId,
      runId,
      message: text,
    });
    try {
      await agent.runAgent({ runId }, {
        onEvent: ({ event }: { event: BaseEvent }) => onEvent("response", event.type, event),
      });
      setMessages([...agent.messages]);
    } catch (cause) {
      setDraft((current) => current || text);
      setError(cause instanceof Error ? cause.message : String(cause));
    } finally {
      setBusy(false);
    }
  };

  const resultByTool = new Map(messages
    .filter((message): message is Extract<AgUiMessage, { role: "tool" }> => message.role === "tool")
    .map((message) => [message.toolCallId, message]));

  return (
    <div className="agent-preview-conversation" hidden={!active}>
      <ChatMessageList
        className="agent-preview-chat__list"
        viewportClassName="agent-preview-chat__viewport"
        ariaLabel={app.t("AG-UI preview conversation", "AG-UI 预览对话")}
        jumpLabel={app.t("Latest", "回到底部")}
        busy={busy}
      >
        {messages.every((message) => message.role !== "user" && message.role !== "assistant") && (
          <EmptyState
            title={app.t("Test through AG-UI", "通过 AG-UI 测试")}
            hint={app.t(
              "The same temporary Session now runs through AG-UI and exposes its lifecycle events below.",
              "同一个临时 Session 将通过 AG-UI 运行，并在下方展示生命周期事件。",
            )}
          />
        )}
        {messages.filter((message) => message.role === "user" || message.role === "assistant").map((message) => {
          const text = agUiMessageText(message);
          const toolCalls = message.role === "assistant" ? (message.toolCalls ?? []) : [];
          return (
            <ChatMessage
              key={message.id}
              role={message.role}
              authorLabel={message.role === "user" ? app.t("You", "你") : displayName}
              body={text && (message.role === "assistant" ? (
                <ChatMarkdown
                  body={text}
                  copyCodeLabel={app.t("Copy code", "复制代码")}
                  copiedCodeLabel={app.t("Copied", "已复制")}
                  copyFailedLabel={app.t("Copy failed", "复制失败")}
                />
              ) : <div style={{ whiteSpace: "pre-wrap" }}>{text}</div>)}
            >
              {toolCalls.map((tool) => {
                const result = resultByTool.get(tool.id);
                return (
                  <section key={tool.id} aria-label={previewToolLabel(tool.function.name, app.locale === "zh")} data-tool={tool.function.name}>
                    <ToolCallCard
                      name={tool.function.name}
                      tone={result?.error ? "error" : result ? "done" : "pending"}
                      statusLabel={result?.error
                        ? app.t("error", "失败")
                        : result
                          ? app.t("done", "已完成")
                          : app.t("awaiting result", "等待结果")}
                      input={tool.function.arguments}
                      output={result?.content ?? result?.error ?? null}
                      defaultOpen={Boolean(result?.error)}
                      labels={{
                        input: app.t("Input", "输入"),
                        output: app.t("Result", "结果"),
                        inputAriaLabel: app.t("Tool input", "工具输入"),
                        outputAriaLabel: app.t("Tool result", "工具结果"),
                      }}
                    />
                  </section>
                );
              })}
            </ChatMessage>
          );
        })}
        {busy && <ChatThinking label={app.t("Agent is working…", "Agent 正在处理…")} />}
      </ChatMessageList>
      {agent && historyReady ? <ChatComposer
        className="transcript-composer agent-preview-chat__composer"
        sendMode="enter"
        value={draft}
        onChange={setDraft}
        onSubmit={() => void submit()}
        onStop={busy ? () => agent.abortRun() : undefined}
        busy={busy}
        ariaLabel={app.t("Message to agent over AG-UI", "通过 AG-UI 给 Agent 的消息")}
        placeholder={app.t("Give this Agent a concrete task…", "给这个 Agent 一个具体任务…")}
        sendLabel={app.t("Send", "发送")}
        stopLabel={app.t("Stop generation", "停止生成")}
        sendIcon={<span>{app.t("Send", "发送")}</span>}
        stopIcon={<span>{app.t("Stop", "停止")}</span>}
        hint={app.t("Enter to send · Shift+Enter for a new line", "Enter 发送 · Shift+Enter 换行")}
      /> : (
        <div className="banner info" role="status">
          <span>↻</span>
          <span>{app.t("Loading the shared Session history for AG-UI…", "正在为 AG-UI 加载同一个 Session 的历史…")}</span>
        </div>
      )}
      {error && <div className="banner err" role="alert"><span>✕</span><span>{error}</span></div>}
    </div>
  );
}
