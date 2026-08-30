import { useMutation } from "@tanstack/react-query";
import { api } from "../lib/api/client";
import { useApp } from "../lib/app-state";
import { Button, Card } from "../components/ui";

export default function A2aSurface() {
  const app = useApp();
  const card = useMutation({
    mutationFn: () => api.get<Record<string, unknown>>("/.well-known/agent-card.json"),
  });
  return (
    <>
      <Card>
        <h2>{app.t("Published Agent Card", "已发布的 Agent Card")}</h2>
        <p className="hint">
          {app.t(
            "Inspect the exact public contract that other A2A clients discover for this Awaken deployment. Configure outbound peers in an Agent's Collaboration settings.",
            "检查其他 A2A 客户端发现当前 Awaken 部署时获得的准确公开契约；出站协作对端请在 Agent 的“协作”设置中配置。",
          )}
        </p>
        <Button variant="primary" disabled={card.isPending} onClick={() => card.mutate()}>
          {card.isPending ? app.t("Loading…", "正在加载…") : app.t("Inspect published card", "检查已发布 Card")}
        </Button>
        {card.data && (
          <pre className="mono" style={{ whiteSpace: "pre-wrap", fontSize: 11.5, marginTop: 10 }}>
            {JSON.stringify(card.data, null, 2)}
          </pre>
        )}
        {card.error instanceof Error && <div className="err">{card.error.message}</div>}
      </Card>
      <Card>
        <h2>{app.t("This Awaken deployment is an A2A Agent", "当前 Awaken 部署是 A2A Agent")}</h2>
        <p className="hint">
          {app.t(
            "Other A2A clients can discover this server from the public card, then send or stream messages. Task status, cancellation, subscriptions, and notifications remain attached to the same Session.",
            "其他 A2A 客户端可通过公开卡片发现此服务，再发送或流式发送消息。任务状态、取消、订阅和通知都归属于同一个 Session。",
          )}
        </p>
        <ul className="protocol-endpoints">
          <li><code>GET /.well-known/agent-card.json</code></li>
          <li><code>POST /v1/a2a/message:send</code></li>
          <li><code>POST /v1/a2a/message:stream</code></li>
          <li><code>POST /v1/a2a</code></li>
        </ul>
      </Card>
    </>
  );
}
