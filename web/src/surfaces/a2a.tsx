import { useMutation } from "@tanstack/react-query";
import { useState } from "react";
import { api } from "../lib/api/client";
import { useApp } from "../lib/app-state";
import { Button, Card } from "../components/ui";

export default function A2aSurface() {
  const app = useApp();
  const [agentId, setAgentId] = useState("");
  const card = useMutation({
    mutationFn: (id: string) => api.get<Record<string, unknown>>(`/v1/delegates/${id}/card`),
  });
  return (
    <>
      <Card>
        <h2>{app.t("Delegate card lookup", "委托卡查询")}</h2>
        <p className="hint">
          {app.t("Inspect a remote A2A delegate's agent card.", "查看远程 A2A 委托的 agent card。")}
        </p>
        <div className="row">
          <input className="input mono" style={{ width: 280 }} placeholder="agent id" value={agentId} onChange={(e) => setAgentId(e.target.value)} />
          <Button variant="primary" disabled={!agentId.trim()} onClick={() => card.mutate(agentId.trim())}>
            {app.t("Fetch card", "获取")}
          </Button>
        </div>
        {card.data && (
          <pre className="mono" style={{ whiteSpace: "pre-wrap", fontSize: 11.5, marginTop: 10 }}>
            {JSON.stringify(card.data, null, 2)}
          </pre>
        )}
        {card.error instanceof Error && <div className="err">{card.error.message}</div>}
      </Card>
      <Card>
        <h2>{app.t("This Awaken server is already an A2A agent", "当前 Awaken 服务已是 A2A Agent")}</h2>
        <p className="hint">
          {app.t(
            "Use the well-known card for discovery, then send or stream a message. Tasks, cancellation, subscriptions and push notification configs share the same runtime.",
            "通过 well-known card 发现服务，再发送或流式发送消息。任务、取消、订阅和推送通知配置共享同一运行时。",
          )}
        </p>
        <div className="stack" style={{ gap: 6 }}>
          <code>GET /.well-known/agent-card.json</code>
          <code>POST /v1/a2a/message:send</code>
          <code>POST /v1/a2a/message:stream</code>
          <code>POST /v1/a2a</code>
        </div>
      </Card>
    </>
  );
}
