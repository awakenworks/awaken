// In-editor Live Preview over the official Vercel AI SDK and AG-UI clients. The
// console's management credential only mints one short-lived, Session-bound
// application token; neither browser transport receives a service credential.

import { useChat } from "@ai-sdk/react";
import {
  DefaultChatTransport,
  getToolName,
  isToolUIPart,
  type DynamicToolUIPart,
  type ToolUIPart,
  type UIMessage,
} from "ai";
import {
  ChatComposer,
  ChatMarkdown,
  ChatMessage,
  ChatMessageList,
  ChatThinking,
  ToolCallCard,
  type ToolCallTone,
} from "@awaken/ui";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { Link } from "react-router";
import {
  BUILTIN_LOCAL_ENVIRONMENT_ID,
  applicationProtocolFetch,
  api,
  createManagedSession,
  IdempotencyScope,
  issueApplicationAccessToken,
  type ApplicationAccessTokenRequest,
  ws,
} from "../../lib/api/client";
import type { AgentConfig, InputBinding } from "../../lib/api/types";
import { useApp } from "../../lib/app-state";
import { Button, Card, EmptyState, Pill } from "../ui";
import {
  AgUiConversation,
  ProtocolEventInspector,
  inspectProtocolSse,
  protocolEventType,
  previewToolLabel,
  retainProtocolEvents,
  type PreviewAccess,
  type PreviewProtocol,
  type PreviewProtocolEvent,
} from "./ProtocolDebugger";

export function uiMessageText(message: UIMessage): string {
  return message.parts
    .filter((part): part is Extract<UIMessage["parts"][number], { type: "text" }> => part.type === "text")
    .map((part) => part.text)
    .join("");
}

export interface PreviewToolView {
  name: string;
  tone: ToolCallTone;
  statusLabel: string;
  input: string | null;
  output: string | null;
}

function previewJson(value: unknown): string | null {
  if (value === undefined) return null;
  if (typeof value === "string") return value;
  try {
    return JSON.stringify(value, null, 2);
  } catch {
    return String(value);
  }
}

/** Keep AI SDK wire states visible but translate them into the same compact tool
 * presentation as a durable Session. Raw protocol envelopes are implementation
 * detail and should not make the preview feel like a debugger. */
export function previewToolView(part: ToolUIPart | DynamicToolUIPart, zh: boolean): PreviewToolView {
  const name = getToolName(part);
  const status = part.state;
  const tone: ToolCallTone = status === "output-error" || status === "output-denied"
    ? "error"
    : status === "output-available"
      ? "done"
      : status === "approval-requested" || status === "approval-responded"
        ? "pending"
        : "running";
  const statusLabel = status === "output-available"
    ? (zh ? "已完成" : "done")
    : status === "output-error"
      ? (zh ? "失败" : "error")
      : status === "output-denied"
        ? (zh ? "已拒绝" : "denied")
        : status === "approval-requested"
          ? (zh ? "等待批准" : "awaiting approval")
          : status === "approval-responded"
            ? (zh ? "已处理批准" : "approval resolved")
            : (zh ? "运行中" : "running");
  return {
    name,
    tone,
    statusLabel,
    input: previewJson("input" in part ? part.input : undefined),
    output: status === "output-available"
      ? previewJson(part.output)
      : status === "output-error"
        ? part.errorText
        : status === "output-denied"
          ? (part.approval.reason ?? (zh ? "操作未获批准。" : "The action was not approved."))
          : null,
  };
}

export function draftPreviewSignature(draft: AgentConfig, resources: InputBinding[]): string {
  return JSON.stringify([draft, resources]);
}

export function draftPreviewRequest(previewId: string, draft: AgentConfig, resources: InputBinding[]) {
  return {
    config: draft,
    resources: {
      agent_id: previewId,
      inputs: resources,
      revision: 1,
    },
  };
}

export function draftPreviewAccessRequest(
  externalThreadId: string,
  managedSessionId: string,
): ApplicationAccessTokenRequest {
  return {
    protocols: ["ai-sdk", "ag-ui"],
    operations: ["thread.run", "thread.messages.read"],
    thread_bindings: [{
      external_thread_id: externalThreadId,
      managed_session_id: managedSessionId,
    }],
    expires_in_seconds: 900,
  };
}

export interface DraftPreviewAttempt {
  signature: string;
  externalThreadId: string;
  nextPreviewId: string;
}

/** Retain every create coordinate while the result is unknown; a changed
 * snapshot is a different preview operation and receives fresh coordinates. */
export function selectDraftPreviewAttempt(
  current: DraftPreviewAttempt | undefined,
  signature: string,
  randomId: () => string = () => crypto.randomUUID(),
): DraftPreviewAttempt {
  return current?.signature === signature
    ? current
    : {
        signature,
        externalThreadId: randomId(),
        nextPreviewId: `preview-${randomId()}`,
      };
}

function ProtocolPreview({
  agentName,
  access,
  stale,
  onNew,
}: {
  agentName?: string | null;
  access: PreviewAccess;
  stale: boolean;
  onNew: () => void;
}) {
  const app = useApp();
  const [protocol, setProtocol] = useState<PreviewProtocol>("ai-sdk");
  const [protocolEvents, setProtocolEvents] = useState<PreviewProtocolEvent[]>([]);
  const eventSequence = useRef(0);
  const [draft, setDraft] = useState("");
  const [latencyMs, setLatencyMs] = useState<number>();
  const sentAt = useRef<number | undefined>(undefined);
  const appendProtocolEvent = useCallback((
    eventProtocol: PreviewProtocol,
    direction: PreviewProtocolEvent["direction"],
    type: string,
    payload: unknown,
  ) => {
    setProtocolEvents((current) => retainProtocolEvents([...current, {
      id: ++eventSequence.current,
      protocol: eventProtocol,
      direction,
      type,
      payload,
    }], 120));
  }, []);
  const inspectFetch = useCallback(async (input: RequestInfo | URL, init?: RequestInit) => {
    let payload: unknown = init?.body;
    if (typeof init?.body === "string") {
      try { payload = JSON.parse(init.body); } catch { /* retain the exact body */ }
    }
    appendProtocolEvent("ai-sdk", "request", "POST_RUN", payload);
    const response = await applicationProtocolFetch(access.token.access_token, input, init);
    appendProtocolEvent("ai-sdk", "response", `HTTP_${response.status}`, { content_type: response.headers.get("content-type") });
    void inspectProtocolSse(response.clone().body, (frame) => {
      appendProtocolEvent("ai-sdk", "response", protocolEventType(frame), frame);
    }).catch((cause) => {
      appendProtocolEvent("ai-sdk", "response", "STREAM_ERROR", cause instanceof Error ? cause.message : String(cause));
    });
    return response;
  }, [appendProtocolEvent]);
  const transport = useMemo(
    () =>
      new DefaultChatTransport({
        api: `/v1/ai-sdk/threads/${encodeURIComponent(access.threadId)}/runs`,
        headers: {
          authorization: `Bearer ${access.token.access_token}`,
        },
        fetch: inspectFetch,
      }),
    [access, inspectFetch],
  );
  const { messages, setMessages, sendMessage, status, error, stop } = useChat({
    id: access.threadId,
    transport,
  });
  const wasBusy = useRef(false);
  const busy = status === "submitted" || status === "streaming";

  useEffect(() => {
    if (wasBusy.current && !busy && sentAt.current != null) {
      setLatencyMs(Date.now() - sentAt.current);
      sentAt.current = undefined;
    }
    wasBusy.current = busy;
  }, [busy]);

  useEffect(() => {
    if (protocol !== "ai-sdk") return;
    let cancelled = false;
    void applicationProtocolFetch(
      access.token.access_token,
      `/v1/ai-sdk/threads/${encodeURIComponent(access.threadId)}/messages`,
    ).then(async (response) => {
      if (!response.ok) throw new Error(await response.text());
      const body = await response.json() as { items?: UIMessage[] };
      if (!cancelled) setMessages(body.items ?? []);
    }).catch(() => undefined);
    return () => { cancelled = true; };
  }, [access, protocol, setMessages]);

  const submit = async () => {
    if (!draft.trim() || busy) return;
    const text = draft;
    setDraft("");
    sentAt.current = Date.now();
    try {
      await sendMessage({ text });
    } catch {
      // A transport failure must not destroy the operator's prompt. Only restore
      // when they have not already started writing the next message.
      setDraft((current) => current || text);
      sentAt.current = undefined;
    }
  };

  const displayName = agentName?.trim() || app.t("Current draft", "当前草稿");

  return (
    <Card className="agent-preview-chat">
      <div className="agent-preview-chat__header">
        <span className="row agent-preview-chat__identity">
          <Pill tone="agent">{displayName}</Pill>
          <Pill tone="ok">{protocol === "ai-sdk" ? "AI SDK" : "AG-UI"}</Pill>
          <span className="mut agent-preview-chat__scope">{app.t("Temporary preview", "临时预览")}</span>
          {latencyMs != null && <Pill tone="neutral">{latencyMs} ms</Pill>}
        </span>
        <Button variant="ghost" style={{ height: 24 }} onClick={onNew}>
          ↻ {app.t("New preview", "新预览")}
        </Button>
      </div>
      <div className="agent-preview-protocol-bar">
        <div className="agent-preview-protocol-switch" role="group" aria-label={app.t("Preview protocol", "预览协议")}>
          {(["ai-sdk", "ag-ui"] as const).map((value) => (
            <Button
              key={value}
              variant={protocol === value ? "primary" : "ghost"}
              style={{ height: 28 }}
              aria-pressed={protocol === value}
              onClick={() => setProtocol(value)}
            >
              {value === "ai-sdk" ? "AI SDK" : "AG-UI"}
            </Button>
          ))}
        </div>
        <span className="mut">
          {app.t("Use AI SDK or AG-UI with the same Session.", "使用 AI SDK 或 AG-UI 调试同一个 Session。")}
        </span>
        <Link className="protocol-help-link" to={`/w/${encodeURIComponent(app.workspaceId)}/protocols#protocol-${protocol}`}>
          {app.t("Connection and API key guide →", "连接与 API Key 指南 →")}
        </Link>
      </div>
      {stale && (
        <div className="banner warn" style={{ marginBottom: 8 }}>
          <span>⚠</span>
          <span>
            {app.t(
              "The draft changed after this preview started. Start a new preview to test the latest edits.",
              "当前草稿在预览开始后有新改动，请新建预览以测试最新内容。",
            )}
          </span>
        </div>
      )}
      <div className="agent-preview-conversation" hidden={protocol !== "ai-sdk"}>
      <ChatMessageList
        className="agent-preview-chat__list"
        viewportClassName="agent-preview-chat__viewport"
        ariaLabel={app.t("Preview conversation", "预览对话")}
        jumpLabel={app.t("Latest", "回到底部")}
        busy={busy}
      >
        {messages.length === 0 && (
          <EmptyState
            title={app.t("Test the behavior you just configured", "测试刚刚配置的行为")}
            hint={app.t(
              "Give the Agent a concrete task. Its answer and tool activity stay inside this temporary preview.",
              "给 Agent 一个具体任务；回答与工具活动只保留在本次临时预览中。",
            )}
          />
        )}
        {messages.map((message) => {
          const text = uiMessageText(message);
          const tools = message.parts.filter(isToolUIPart);
          return (
            <ChatMessage
              key={message.id}
              role={message.role === "user" ? "user" : "assistant"}
              authorLabel={message.role === "user" ? app.t("You", "你") : displayName}
              body={text && (message.role === "assistant" ? (
                <ChatMarkdown
                  body={text}
                  copyCodeLabel={app.t("Copy code", "复制代码")}
                  copiedCodeLabel={app.t("Copied", "已复制")}
                  copyFailedLabel={app.t("Copy failed", "复制失败")}
                />
              ) : (
                <div style={{ whiteSpace: "pre-wrap" }}>{text}</div>
              ))}
            >
              {tools.map((tool) => {
                const view = previewToolView(tool, app.locale === "zh");
                return (
                  <section key={tool.toolCallId} aria-label={previewToolLabel(view.name, app.locale === "zh")} data-tool={view.name}>
                    <ToolCallCard
                      {...view}
                      defaultOpen={view.tone === "error" || view.tone === "pending"}
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
      <ChatComposer
        className="transcript-composer agent-preview-chat__composer"
        sendMode="enter"
        value={draft}
        onChange={setDraft}
        onSubmit={() => void submit()}
        onStop={busy ? () => void stop() : undefined}
        busy={busy}
        ariaLabel={app.t("Message to agent", "给 Agent 的消息")}
        placeholder={app.t("Give this Agent a concrete task…", "给这个 Agent 一个具体任务…")}
        sendLabel={app.t("Send", "发送")}
        stopLabel={app.t("Stop generation", "停止生成")}
        sendIcon={<span>{app.t("Send", "发送")}</span>}
        stopIcon={<span>{app.t("Stop", "停止")}</span>}
        hint={app.t("Enter to send · Shift+Enter for a new line", "Enter 发送 · Shift+Enter 换行")}
      />
      {error && (
        <div className="banner err" role="alert">
          <span>✕</span>
          <span>
            <strong>{app.t("Preview failed", "预览失败")}</strong><br />
            {error.message}<br />
            <span className="mut">{app.t("Your message is kept in the composer so you can retry.", "消息已保留在输入框中，可以直接重试。")}</span>
          </span>
        </div>
      )}
      </div>
      <AgUiConversation
        access={access}
        agentName={agentName}
        active={protocol === "ag-ui"}
        onEvent={(direction, type, payload) => appendProtocolEvent("ag-ui", direction, type, payload)}
      />
      <ProtocolEventInspector
        events={protocolEvents.filter((event) => event.protocol === protocol)}
        onClear={() => setProtocolEvents((current) => current.filter((event) => event.protocol !== protocol))}
      />
    </Card>
  );
}

export default function SandboxPane({
  draft,
  resources,
  canPreview,
}: {
  draft: AgentConfig;
  resources: InputBinding[];
  canPreview: boolean;
}) {
  const app = useApp();
  const previewId = useRef<string | undefined>(undefined);
  const previewAttempt = useRef<DraftPreviewAttempt | undefined>(undefined);
  const createIdentity = useRef(new IdempotencyScope("preview-session-create"));
  const signature = useMemo(() => draftPreviewSignature(draft, resources), [draft, resources]);
  const [access, setAccess] = useState<PreviewAccess>();
  const [previewedSignature, setPreviewedSignature] = useState<string>();
  const [starting, setStarting] = useState(false);
  const [startError, setStartError] = useState<string>();

  const start = async () => {
    setStarting(true);
    setStartError(undefined);
    const attempt = selectDraftPreviewAttempt(previewAttempt.current, signature);
    previewAttempt.current = attempt;
    const { externalThreadId, nextPreviewId } = attempt;
    let registered = false;
    try {
      await api.post(
        ws(`/v1/config/agent-previews/${nextPreviewId}`),
        draftPreviewRequest(nextPreviewId, draft, resources),
      );
      registered = true;
      const request = {
        agent: nextPreviewId,
        environment_id: BUILTIN_LOCAL_ENVIRONMENT_ID,
      };
      const session = await createManagedSession(request, createIdentity.current);
      const token = await issueApplicationAccessToken(draftPreviewAccessRequest(externalThreadId, session.id));
      const previousPreviewId = previewId.current;
      previewId.current = nextPreviewId;
      previewAttempt.current = undefined;
      createIdentity.current.complete();
      setAccess({ token, threadId: externalThreadId, agentId: nextPreviewId });
      setPreviewedSignature(signature);
      if (previousPreviewId) {
        void api.del(ws(`/v1/config/agent-previews/${previousPreviewId}`)).catch(() => undefined);
      }
    } catch (error) {
      if (registered) {
        void api.del(ws(`/v1/config/agent-previews/${nextPreviewId}`)).catch(() => undefined);
      }
      setStartError(error instanceof Error ? error.message : String(error));
    } finally {
      setStarting(false);
    }
  };

  useEffect(() => () => {
    if (previewId.current) {
      void api.del(ws(`/v1/config/agent-previews/${previewId.current}`)).catch(() => undefined);
    }
  }, []);

  if (!canPreview) {
    return (
      <EmptyState
        title={app.t("Complete the runnable fields to Try", "补全运行必填项后即可试运行")}
        hint={app.t(
          "A system prompt and a resolvable model are required. Saving and publishing are not required.",
          "需要填写系统提示词并选择可解析的模型；无需保存或发布。",
        )}
      />
    );
  }

  if (!access) {
    return (
      <EmptyState
        title={app.t("Try the current draft", "试运行当前草稿")}
        hint={app.t(
          "Awaken compiles an isolated, temporary snapshot of the current fields and resources. It is never saved or published as an Agent.",
          "Awaken 会把当前字段和资源编译为隔离的临时快照，不会保存或发布为正式 Agent。",
        )}
        action={
          <div className="col" style={{ gap: 8, alignItems: "center" }}>
            <Button variant="primary" disabled={starting} onClick={() => void start()}>
              {starting ? app.t("Authorizing…", "正在授权…") : app.t("Start preview", "开始预览")}
            </Button>
            {startError && <div className="err">{startError}</div>}
          </div>
        }
      />
    );
  }

  return (
    <ProtocolPreview
      key={access.threadId}
      agentName={draft.name}
      access={access}
      stale={previewedSignature !== signature}
      onNew={() => void start()}
    />
  );
}
