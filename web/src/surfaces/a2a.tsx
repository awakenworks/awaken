import { useMutation } from "@tanstack/react-query";
import { useState } from "react";
import { api } from "../lib/api/client";
import { useApp } from "../lib/app-state";

export default function A2aSurface() {
  const app = useApp();
  const [agentId, setAgentId] = useState("");
  const card = useMutation({
    mutationFn: (id: string) => api.get<Record<string, unknown>>(`/v1/delegates/${id}/card`),
  });
  return (
    <>
      <div className="card">
        <h2>{app.t("Delegate card lookup", "委托卡查询")}</h2>
        <p className="hint">
          {app.t("Inspect a remote A2A delegate's agent card.", "查看远程 A2A 委托的 agent card。")}
        </p>
        <div className="row">
          <input className="input mono" style={{ width: 280 }} placeholder="agent id" value={agentId} onChange={(e) => setAgentId(e.target.value)} />
          <button className="btn primary" disabled={!agentId.trim()} onClick={() => card.mutate(agentId.trim())}>
            {app.t("Fetch card", "获取")}
          </button>
        </div>
        {card.data && (
          <pre className="mono" style={{ whiteSpace: "pre-wrap", fontSize: 11.5, marginTop: 10 }}>
            {JSON.stringify(card.data, null, 2)}
          </pre>
        )}
        {card.error instanceof Error && <div className="err">{card.error.message}</div>}
      </div>
      <div className="banner gate">
        <span>◌</span>
        <span>{app.t("A2A server CRUD is a roadmap item (§7.9).", "A2A 服务器 CRUD 是路线项(§7.9)。")}</span>
      </div>
    </>
  );
}
