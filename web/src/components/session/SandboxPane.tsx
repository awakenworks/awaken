// In-editor Live Preview over the official Vercel AI SDK. The console's
// management credential is used only to mint a short-lived application token;
// `useChat` receives that narrow token and never receives the service credential.

import { useChat } from "@ai-sdk/react";
import { DefaultChatTransport, type UIMessage } from "ai";
import { useEffect, useMemo, useRef, useState } from "react";
import {
  getWorkspace,
  issueApplicationAccessToken,
  type IssuedApplicationAccessToken,
} from "../../lib/api/client";
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

function AiSdkPreview({
  agentId,
  access,
  dirty,
  onNew,
}: {
  agentId: string;
  access: PreviewAccess;
  dirty: boolean;
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
      {dirty && (
        <div className="banner warn" style={{ marginBottom: 8 }}>
          <span>⚠</span>
          <span>
            {app.t(
              "Unsaved edits aren't live yet — Save + Publish to test them.",
              "未保存的修改尚未生效——保存并发布后再测试。",
            )}
          </span>
        </div>
      )}
      <div className="transcript" style={{ display: "flex", flexDirection: "column", gap: 8 }}>
        {messages.length === 0 && (
          <div className="mut" style={{ padding: "18px 4px", textAlign: "center" }}>
            {app.t("Ask the published Agent anything.", "向已发布的 Agent 提问。")}
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

/** Enabled only for a published, saved agent — an unpublished draft is not
 * installed in the runtime, so there is nothing live to talk to yet. */
export default function SandboxPane({
  agentId,
  ready,
  dirty,
}: {
  agentId: string;
  ready: boolean;
  dirty: boolean;
}) {
  const app = useApp();
  const [access, setAccess] = useState<PreviewAccess>();
  const [starting, setStarting] = useState(false);
  const [startError, setStartError] = useState<string>();

  const start = async () => {
    setStarting(true);
    setStartError(undefined);
    const threadId = crypto.randomUUID();
    try {
      const token = await issueApplicationAccessToken({
        authority_id: "awaken-console",
        application_scope: previewApplicationScope(getWorkspace()),
        thread_namespace: "live-preview",
        operations: ["thread.run", "thread.read"],
        agent_ids: [agentId],
        default_agent_id: agentId,
        expires_in_seconds: 900,
      });
      setAccess({ token, threadId });
    } catch (error) {
      setStartError(error instanceof Error ? error.message : String(error));
    } finally {
      setStarting(false);
    }
  };

  if (!ready) {
    return (
      <EmptyState
        title={app.t("Publish to test in Live Preview", "发布后即可实时预览")}
        hint={app.t(
          "Live Preview runs the installed Agent through the authenticated AI SDK endpoint.",
          "实时预览通过带应用鉴权的 AI SDK 接口运行已安装的 Agent。",
        )}
      />
    );
  }

  if (!access) {
    return (
      <EmptyState
        title={app.t("Start an authenticated Live Preview", "开始带鉴权的实时预览")}
        hint={app.t(
          "Awaken exchanges your management credential for a 15-minute, Agent-scoped application token kept only in this tab.",
          "Awaken 会把管理凭证换成仅限当前 Agent、有效 15 分钟且只保存在当前标签页的应用令牌。",
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
      agentId={agentId}
      access={access}
      dirty={dirty}
      onNew={() => void start()}
    />
  );
}
