import { useMutation } from "@tanstack/react-query";
import { useState } from "react";
import { api, ws } from "../lib/api/client";
import { useApp } from "../lib/app-state";
import { Button, Card, TextField } from "../components/ui";

export default function A2aSurface() {
  const app = useApp();
  const [agentId, setAgentId] = useState("");
  const card = useMutation({
    mutationFn: (id: string) => api.get<Record<string, unknown>>(ws(`/v1/delegates/${id}/card`)),
  });
  return (
    <>
      <Card>
        <h2>{app.t("Delegate card lookup", "委托卡查询")}</h2>
        <p className="hint">
          {app.t("Enter a configured delegate ID to inspect what the remote Agent advertises before using it.", "输入已配置的委托 ID，在使用前查看远程 Agent 声明的能力。")}
        </p>
        <div className="row">
          <TextField label={app.t("Delegate ID", "委托 ID")} mono style={{ width: 280 }} placeholder="research-partner" value={agentId} onChange={(e) => setAgentId(e.target.value)} />
          <Button variant="primary" disabled={!agentId.trim() || card.isPending} onClick={() => card.mutate(agentId.trim())}>
            {card.isPending ? app.t("Loading…", "正在加载…") : app.t("View capabilities", "查看能力")}
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
            "Other A2A clients can discover this server from the public card, then send or stream messages. Task status, cancellation, subscriptions, and notifications remain attached to the same Session.",
            "其他 A2A 客户端可通过公开卡片发现此服务，再发送或流式发送消息。任务状态、取消、订阅和通知都归属于同一个 Session。",
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
