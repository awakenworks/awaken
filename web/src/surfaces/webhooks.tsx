import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useState } from "react";
import { useApp } from "../lib/app-state";
import { api, ws } from "../lib/api/client";
import {
  Button,
  Card,
  CopyButton,
  EmptyState,
  Pill,
  TechnicalId,
  TextAreaField,
  TextField,
  useConfirm,
  useToast,
} from "../components/ui";

export interface WebhookSubscription {
  id: string;
  workspace_id: string;
  url: string;
  event_types: string[];
  disabled: boolean;
  consecutive_failures: number;
}

interface WebhookList {
  data: WebhookSubscription[];
  has_more: boolean;
}

interface CreatedWebhook extends WebhookSubscription {
  secret: string;
}

export type WebhookHealth = "active" | "degraded" | "paused" | "delivery_failed";

/** Delivery state is operational evidence, not a synonym for the authored
 * enabled switch. Keep a manually paused endpoint distinct from one disabled
 * after repeated delivery failures. */
export function webhookHealth(subscription: Pick<WebhookSubscription, "disabled" | "consecutive_failures">): WebhookHealth {
  if (subscription.disabled) return subscription.consecutive_failures > 0 ? "delivery_failed" : "paused";
  return subscription.consecutive_failures > 0 ? "degraded" : "active";
}

export function parseEventTypes(value: string): string[] {
  return [...new Set(value.split(/[\n,]/).map((item) => item.trim()).filter(Boolean))];
}

function newWebhookId(): string {
  return `wh_${globalThis.crypto.randomUUID().replaceAll("-", "")}`;
}

function healthLabel(health: WebhookHealth, t: (en: string, zh: string) => string): string {
  switch (health) {
    case "active": return t("Active", "正常");
    case "degraded": return t("Delivery retrying", "投递重试中");
    case "paused": return t("Paused", "已暂停");
    case "delivery_failed": return t("Disabled after failures", "失败后已停用");
  }
}

export default function WebhooksSurface() {
  const app = useApp();
  const qc = useQueryClient();
  const toast = useToast();
  const confirm = useConfirm();
  const queryKey = ["webhook-subscriptions", app.workspaceId];
  const subscriptions = useQuery({
    queryKey,
    queryFn: () => api.get<WebhookList>(ws("/v1/config/webhook-subscriptions")),
    retry: false,
  });
  const [url, setUrl] = useState("");
  const [events, setEvents] = useState("");
  const [created, setCreated] = useState<CreatedWebhook | null>(null);

  const create = useMutation({
    mutationFn: () => api.put<CreatedWebhook>(ws(`/v1/config/webhook-subscriptions/${newWebhookId()}`), {
      url: url.trim(),
      event_types: parseEventTypes(events),
    }),
    onSuccess: (result) => {
      setCreated(result);
      setUrl("");
      setEvents("");
      void qc.invalidateQueries({ queryKey });
      toast.ok(app.t("Webhook endpoint created.", "Webhook 端点已创建。"));
    },
    onError: (cause) => toast.err(cause instanceof Error ? cause.message : String(cause)),
  });
  const update = useMutation({
    mutationFn: ({ subscription, disabled }: { subscription: WebhookSubscription; disabled: boolean }) =>
      api.put<WebhookSubscription>(ws(`/v1/config/webhook-subscriptions/${encodeURIComponent(subscription.id)}`), {
        url: subscription.url,
        event_types: subscription.event_types,
        disabled,
      }),
    onSuccess: (_, variables) => {
      void qc.invalidateQueries({ queryKey });
      toast.ok(variables.disabled
        ? app.t("Webhook paused.", "Webhook 已暂停。")
        : app.t("Webhook enabled. New events will be delivered.", "Webhook 已启用，将投递之后产生的新事件。"));
    },
    onError: (cause) => toast.err(cause instanceof Error ? cause.message : String(cause)),
  });
  const remove = useMutation({
    mutationFn: (id: string) => api.del(ws(`/v1/config/webhook-subscriptions/${encodeURIComponent(id)}`)),
    onSuccess: () => {
      void qc.invalidateQueries({ queryKey });
      toast.ok(app.t("Webhook deleted.", "Webhook 已删除。"));
    },
    onError: (cause) => toast.err(cause instanceof Error ? cause.message : String(cause)),
  });

  const deleteWebhook = async (subscription: WebhookSubscription) => {
    const approved = await confirm({
      title: app.t("Delete this webhook?", "删除这个 Webhook？"),
      body: app.t(
        "Future events will stop immediately and the signing secret cannot be recovered. This cannot be undone.",
        "之后产生的事件将立即停止投递，签名密钥也无法恢复。此操作不可撤销。",
      ),
      confirmLabel: app.t("Delete webhook", "删除 Webhook"),
      danger: true,
    });
    if (approved) remove.mutate(subscription.id);
  };

  return (
    <div className="stack">
      {created && (
        <div className="banner warn webhook-secret-banner">
          <span>🔑</span>
          <span className="webhook-secret-copy">
            <strong>{app.t("Signing secret shown once", "签名密钥仅显示一次")}</strong>
            <code>{created.secret}</code>
            <small>{app.t("Store it as ANTHROPIC_WEBHOOK_SIGNING_KEY in your receiver. Awaken cannot show it again.", "请在接收端保存为 ANTHROPIC_WEBHOOK_SIGNING_KEY；Awaken 不会再次显示。")}</small>
          </span>
          <CopyButton value={created.secret} label={app.t("Copy secret", "复制密钥")} copiedLabel={app.t("Copied", "已复制")} />
          <Button variant="ghost" aria-label={app.t("Dismiss", "关闭")} onClick={() => setCreated(null)}>✕</Button>
        </div>
      )}

      <Card>
        <h2>{app.t("Send lifecycle events to your backend", "向你的后端发送生命周期事件")}</h2>
        <p className="hint">
          {app.t(
            "Awaken signs and sends major Agent and Session state changes to a public HTTPS endpoint. Webhooks complement the live Session stream; they are not an inbound protocol for calling an Agent.",
            "Awaken 会签名并把重要的 Agent 与 Session 状态变化发送到公网 HTTPS 端点。Webhook 用于补充实时 Session 事件流，不是调用 Agent 的入站协议。",
          )}
        </p>
        <ol className="protocol-endpoints">
          <li>{app.t("Create the endpoint and copy its signing secret once.", "创建端点，并立即复制只显示一次的签名密钥。")}</li>
          <li>{app.t("Verify webhook-id, webhook-timestamp, and webhook-signature before parsing the body.", "解析正文前，先校验 webhook-id、webhook-timestamp 与 webhook-signature。")}</li>
          <li>{app.t("Deduplicate by the stable event id, fetch the referenced object, then return any 2xx response.", "按稳定 event id 去重，获取事件引用的对象，再返回任意 2xx 响应。")}</li>
        </ol>
        <a className="protocol-help-link" href="https://platform.claude.com/docs/en/managed-agents/webhooks" target="_blank" rel="noreferrer">
          {app.t("Read the compatible Managed Agents webhook guide ↗", "阅读兼容的 Managed Agents Webhook 指南 ↗")}
        </a>
      </Card>

      <Card>
        <h2>{app.t("Create webhook endpoint", "创建 Webhook 端点")}</h2>
        <div className="webhook-create-grid">
          <TextField
            label={app.t("Public HTTPS URL", "公网 HTTPS URL")}
            value={url}
            onChange={(event) => setUrl(event.target.value)}
            placeholder="https://events.example.com/awaken"
            type="url"
            autoComplete="url"
            hint={app.t("Private, loopback, metadata, and non-HTTPS addresses are rejected.", "私有、回环、元数据地址及非 HTTPS 地址会被拒绝。")}
          />
          <TextAreaField
            label={app.t("Event types · optional", "事件类型 · 可选")}
            value={events}
            onChange={(event) => setEvents(event.target.value)}
            rows={3}
            mono
            placeholder={"session.status_idled\nsession.status_terminated"}
            hint={app.t("Leave empty for every lifecycle event emitted by this Awaken deployment. Separate exact webhook event names with commas or new lines; SSE names may differ.", "留空表示订阅当前 Awaken 部署发出的全部生命周期事件。用逗号或换行分隔准确的 Webhook 事件名；SSE 名称可能不同。")}
          />
        </div>
        <div className="row" style={{ justifyContent: "flex-end" }}>
          <Button variant="primary" disabled={create.isPending || !url.trim()} onClick={() => create.mutate()}>
            {create.isPending ? app.t("Creating…", "正在创建…") : app.t("Create endpoint", "创建端点")}
          </Button>
        </div>
      </Card>

      <Card className="responsive-table-card" style={{ padding: 0 }}>
        <table className="table">
          <thead>
            <tr>
              <th>{app.t("Endpoint", "端点")}</th>
              <th>{app.t("Events", "事件")}</th>
              <th>{app.t("Delivery", "投递状态")}</th>
              <th />
            </tr>
          </thead>
          <tbody>
            {(subscriptions.data?.data ?? []).map((subscription) => {
              const health = webhookHealth(subscription);
              return (
                <tr key={subscription.id}>
                  <td data-label={app.t("Endpoint", "端点")}>
                    <strong className="webhook-url">{subscription.url}</strong>
                    <TechnicalId value={subscription.id} />
                  </td>
                  <td data-label={app.t("Events", "事件")}>
                    {subscription.event_types.length === 0
                      ? <span className="mut">{app.t("All emitted lifecycle events", "全部已发出的生命周期事件")}</span>
                      : <div className="webhook-event-list">{subscription.event_types.map((event) => <code key={event}>{event}</code>)}</div>}
                  </td>
                  <td data-label={app.t("Delivery", "投递状态")}>
                    <Pill tone={health === "active" ? "ok" : health === "paused" ? "neutral" : "warn"}>{healthLabel(health, app.t)}</Pill>
                    {subscription.consecutive_failures > 0 && (
                      <small className="mut">{app.t(`${subscription.consecutive_failures} consecutive failures`, `连续失败 ${subscription.consecutive_failures} 次`)}</small>
                    )}
                  </td>
                  <td className="responsive-table-actions">
                    <div className="row" style={{ justifyContent: "flex-end" }}>
                      <Button
                        disabled={update.isPending && update.variables?.subscription.id === subscription.id}
                        onClick={() => update.mutate({ subscription, disabled: !subscription.disabled })}
                      >
                        {subscription.disabled ? app.t("Enable", "启用") : app.t("Pause", "暂停")}
                      </Button>
                      <Button variant="danger" disabled={remove.isPending && remove.variables === subscription.id} onClick={() => void deleteWebhook(subscription)}>
                        {app.t("Delete", "删除")}
                      </Button>
                    </div>
                  </td>
                </tr>
              );
            })}
            {!subscriptions.isLoading && (subscriptions.data?.data.length ?? 0) === 0 && (
              <tr><td colSpan={4}><EmptyState title={app.t("No webhook endpoints", "暂无 Webhook 端点")} hint={app.t("Create one when an external backend needs durable lifecycle notifications without polling.", "当外部后端需要无需轮询的持久生命周期通知时，再创建端点。")}/></td></tr>
            )}
          </tbody>
        </table>
        {subscriptions.isLoading && <p className="mut" style={{ padding: 16 }}>{app.t("Loading webhook endpoints…", "正在加载 Webhook 端点…")}</p>}
        {subscriptions.error instanceof Error && <div className="err" style={{ margin: 16 }}>{subscriptions.error.message}</div>}
      </Card>
    </div>
  );
}
