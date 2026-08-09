// In-editor Live Preview over the official Vercel AI SDK. The console's
// management credential is used only to mint a short-lived application token;
// `useChat` receives that narrow token and never receives the service credential.

import { useChat } from "@ai-sdk/react";
import { DefaultChatTransport, type UIMessage } from "ai";
import { useEffect, useMemo, useRef, useState } from "react";
import {
  api,
  getWorkspace,
  issueApplicationAccessToken,
  type IssuedApplicationAccessToken,
  ws,
} from "../../lib/api/client";
import type { AgentConfig, InputBinding, Session } from "../../lib/api/types";
import { useApp } from "../../lib/app-state";
import { Button, Card, EmptyState, Pill } from "../ui";

export interface PreviewAccess {
  token: IssuedApplicationAccessToken;
  threadId: string;
}

export function previewApplicationScope(workspace: string): string {
  return `console:${workspace.trim() || "default"}`;
}

export function uiMessageText(message: UIMessage): string {
  return message.parts
    .filter((part): part is Extract<UIMessage["parts"][number], { type: "text" }> => part.type === "text")
    .map((part) => part.text)
    .join("");
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

function AiSdkPreview({
  agentId,
  access,
  stale,
  onNew,
}: {
  agentId: string;
  access: PreviewAccess;
  stale: boolean;
  onNew: () => void;
}) {
  const app = useApp();
  const [draft, setDraft] = useState("");
  const [latencyMs, setLatencyMs] = useState<number>();
  const sentAt = useRef<number | undefined>(undefined);
  const transport = useMemo(
    () =>
      new DefaultChatTransport({
        api: `/v1/ai-sdk/threads/${encodeURIComponent(access.threadId)}/runs`,
        headers: {
          authorization: `Bearer ${access.token.access_token}`,
        },
      }),
    [access],
  );
  const { messages, sendMessage, status, error, stop } = useChat({
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

  const submit = async () => {
    if (!draft.trim() || busy) return;
    const text = draft;
    setDraft("");
    sentAt.current = Date.now();
    await sendMessage({ text });
  };

  return (
    <Card style={{ padding: "12px 14px" }}>
      <div className="row" style={{ justifyContent: "space-between", marginBottom: 8 }}>
        <span className="row">
          <Pill tone="agent">{agentId}</Pill>
          <Pill tone="ok">AI SDK</Pill>
          <code className="mut" style={{ fontSize: 11 }}>
            {access.threadId}
          </code>
          {latencyMs != null && <Pill tone="neutral">{latencyMs} ms</Pill>}
        </span>
        <Button variant="ghost" style={{ height: 24 }} onClick={onNew}>
          ↻ {app.t("New preview", "新预览")}
        </Button>
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
      <div className="transcript" style={{ display: "flex", flexDirection: "column", gap: 8 }}>
        {messages.length === 0 && (
          <div className="mut" style={{ padding: "18px 4px", textAlign: "center" }}>
            {app.t("Ask the current Agent draft anything.", "向当前 Agent 草稿提问。")}
          </div>
        )}
        {messages.map((message) => {
          const text = uiMessageText(message);
          const tools = message.parts.filter((part) => part.type.startsWith("tool-"));
          return (
            <Card
              key={message.id}
              style={{
                padding: "9px 12px",
                maxWidth: message.role === "user" ? "88%" : "92%",
                alignSelf: message.role === "user" ? "flex-end" : "flex-start",
                background: message.role === "user" ? "var(--soft)" : undefined,
              }}
            >
              <span className="mut" style={{ fontSize: 10.5 }}>
                {message.role === "user" ? "you" : "⬡ agent"}
              </span>
              {text && <div style={{ whiteSpace: "pre-wrap", lineHeight: 1.55 }}>{text}</div>}
              {tools.map((tool, index) => (
                <details key={`${message.id}-tool-${index}`} style={{ marginTop: 6 }}>
                  <summary className="mono" style={{ cursor: "pointer", fontSize: 11 }}>
                    🛠 {tool.type.replace(/^tool-/, "")}
                  </summary>
                  <pre className="mono" style={{ whiteSpace: "pre-wrap", fontSize: 11 }}>
                    {JSON.stringify(tool, null, 2)}
                  </pre>
                </details>
              ))}
            </Card>
          );
        })}
        {busy && (
          <div className="agent-working row" role="status" aria-live="polite">
            <span className="dot pulse" style={{ background: "var(--agent)" }} />
            <span>{app.t("Agent is working…", "Agent 正在处理…")}</span>
          </div>
        )}
        <div className="transcript-composer">
          <textarea
            className="input transcript-composer__input"
            rows={1}
            aria-label={app.t("Message to agent", "给 Agent 的消息")}
            placeholder={app.t("Ask the agent…", "问问这个 Agent…")}
            value={draft}
            onChange={(event) => setDraft(event.target.value)}
            onKeyDown={(event) => {
              if (event.key === "Enter" && !event.shiftKey && !event.nativeEvent.isComposing) {
                event.preventDefault();
                void submit();
              }
            }}
          />
          {busy ? (
            <Button onClick={() => void stop()}>{app.t("Stop", "停止")}</Button>
          ) : (
            <Button variant="primary" disabled={!draft.trim()} onClick={() => void submit()}>
              {app.t("Send", "发送")}
            </Button>
          )}
        </div>
        {error && <div className="err">{error.message}</div>}
      </div>
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
  const signature = useMemo(() => draftPreviewSignature(draft, resources), [draft, resources]);
  const [access, setAccess] = useState<PreviewAccess>();
  const [previewedSignature, setPreviewedSignature] = useState<string>();
  const [starting, setStarting] = useState(false);
  const [startError, setStartError] = useState<string>();

  const start = async () => {
    setStarting(true);
    setStartError(undefined);
    const externalThreadId = crypto.randomUUID();
    const nextPreviewId = `preview-${crypto.randomUUID()}`;
    let registered = false;
    try {
      await api.post(
        ws(`/v1/config/agent-previews/${nextPreviewId}`),
        draftPreviewRequest(nextPreviewId, draft, resources),
      );
      registered = true;
      const session = await api.post<Session>(ws("/v1/sessions"), { agent: nextPreviewId });
      const token = await issueApplicationAccessToken({
        authority_id: "awaken-console",
        application_scope: previewApplicationScope(getWorkspace()),
        protocols: ["ai-sdk"],
        operations: ["thread.run", "thread.messages.read"],
        thread_bindings: [{
          external_thread_id: externalThreadId,
          managed_session_id: session.id,
        }],
        expires_in_seconds: 900,
      });
      const previousPreviewId = previewId.current;
      previewId.current = nextPreviewId;
      setAccess({ token, threadId: externalThreadId });
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
    <AiSdkPreview
      key={access.threadId}
      agentId={draft.id.trim() || app.t("new draft", "新草稿")}
      access={access}
      stale={previewedSignature !== signature}
      onNew={() => void start()}
    />
  );
}
