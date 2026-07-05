// Workspace · Agents: the publish pipeline (draft → validate → publish) plus
// the workspace-level MCP binding. The richer editor tabs (History, plugins
// schema forms, permission preview) land with their backend faces (§7).

import { useMutation, useQuery } from "@tanstack/react-query";
import { useEffect, useState } from "react";
import { api, isAbsent } from "../lib/api/client";
import type { AgentMcpConfig, McpServerDef, ResolvedMcpServerView } from "../lib/api/types";
import { useApp } from "../lib/app-state";

const WORKSPACE = "wrkspc_default";

const DRAFT_TEMPLATE = `{
  "instructions": "You are a helpful coding agent.",
  "model": "claude-sonnet-4-5"
}`;

export default function AgentsSurface() {
  const app = useApp();
  const [agentId, setAgentId] = useState("default");
  const [draft, setDraft] = useState(DRAFT_TEMPLATE);
  const validate = useMutation({
    mutationFn: () =>
      api.post<{ valid: boolean; error?: string }>(
        `/v1/config/agents/${agentId}/validate`,
        JSON.parse(draft),
      ),
  });
  const publish = useMutation({
    mutationFn: async () => {
      await api.put(`/v1/config/agents/${agentId}`, JSON.parse(draft));
      return api.post<{ publication_id: string; fingerprint: string; installed: boolean }>(
        `/v1/config/agents/${agentId}/publish`,
        JSON.parse(draft),
      );
    },
  });
  const publishAbsent =
    (validate.isError && isAbsent(validate.error)) || (publish.isError && isAbsent(publish.error));

  // Workspace-level MCP binding (always available on the admin plane).
  const servers = useQuery({
    queryKey: ["mcp-servers"],
    queryFn: () => api.get<McpServerDef[]>("/v1/config/mcp-servers"),
  });
  const binding = useQuery({
    queryKey: ["agent-mcp", agentId],
    queryFn: () => api.get<AgentMcpConfig>(`/v1/config/agents/${agentId}/mcp`),
    retry: false,
  });
  const [selected, setSelected] = useState<string[]>([]);
  useEffect(() => {
    setSelected(binding.data?.mcp_server_ids ?? []);
  }, [binding.data]);
  const saveBinding = useMutation({
    mutationFn: () =>
      api.put<AgentMcpConfig>(`/v1/config/agents/${agentId}/mcp`, {
        agent_id: agentId,
        mcp_server_ids: selected,
        version: (binding.data?.version ?? 0) + 1,
      }),
    onSuccess: () => void binding.refetch(),
  });
  const resolve = useMutation({
    mutationFn: () =>
      api.post<ResolvedMcpServerView[]>(`/v1/config/agents/${agentId}/mcp/resolve`, {
        workspace_id: WORKSPACE,
      }),
  });

  return (
    <>
      <div className="row">
        <span className="field">
          <label>Agent id</label>
          <input className="input mono" value={agentId} onChange={(e) => setAgentId(e.target.value)} />
        </span>
      </div>

      <div className="card">
        <h2>{app.t("Draft → Validate → Publish", "草稿 → 校验 → 发布")}</h2>
        <textarea className="input mono" rows={8} value={draft} onChange={(e) => setDraft(e.target.value)} />
        <div className="row" style={{ marginTop: 10 }}>
          <button className="btn ghost" disabled={validate.isPending} onClick={() => validate.mutate()}>
            {app.t("Validate", "校验")}
          </button>
          <button className="btn primary" disabled={publish.isPending} onClick={() => publish.mutate()}>
            {app.t("Publish", "发布")} ➤
          </button>
          {validate.data && (
            <span className={`pill ${validate.data.valid ? "ok" : "danger"}`}>
              {validate.data.valid ? "valid ✓" : `invalid: ${validate.data.error ?? ""}`}
            </span>
          )}
          {publish.data && (
            <span className="pill ok">
              published · <code>{publish.data.fingerprint.slice(0, 12)}…</code>
            </span>
          )}
        </div>
        {publishAbsent && (
          <div className="banner gate" style={{ marginTop: 10 }}>
            <span>◌</span>
            <span>
              {app.t(
                "The config publish plane is not mounted in this server mode (AWAKEN_MODEL_MODE=config mounts it).",
                "当前服务模式未挂载 config 发布面(AWAKEN_MODEL_MODE=config 才挂载)。",
              )}
            </span>
          </div>
        )}
        {!publishAbsent && validate.error instanceof Error && <div className="err">{validate.error.message}</div>}
        {!publishAbsent && publish.error instanceof Error && <div className="err">{publish.error.message}</div>}
      </div>

      <div className="card">
        <h2>{app.t("Workspace MCP binding", "工作区级 MCP 绑定")}</h2>
        {(servers.data ?? []).map((s) => (
          <label key={s.id} className="row" style={{ padding: "6px 2px", cursor: "pointer" }}>
            <input
              type="checkbox"
              checked={selected.includes(s.id)}
              onChange={(e) =>
                setSelected(e.target.checked ? [...selected, s.id] : selected.filter((x) => x !== s.id))
              }
            />
            <code>{s.id}</code>
            <span className="mono mut" style={{ marginLeft: "auto", fontSize: 11 }}>
              {s.url}
            </span>
          </label>
        ))}
        <div className="row" style={{ marginTop: 8 }}>
          <button className="btn primary" disabled={saveBinding.isPending} onClick={() => saveBinding.mutate()}>
            {app.t("Save binding", "保存绑定")}
          </button>
          <button className="btn ghost" onClick={() => resolve.mutate()}>
            Resolve
          </button>
          {resolve.data && (
            <span className="chain">
              {resolve.data.map((r) => (
                <span key={r.url} className="chip">
                  {r.name}
                  <span className="dot" style={{ background: r.credential_present ? "var(--ok)" : "var(--fg3)" }} />
                </span>
              ))}
            </span>
          )}
        </div>
        {saveBinding.error instanceof Error && <div className="err">{saveBinding.error.message}</div>}
        {resolve.error instanceof Error && <div className="err">{resolve.error.message}</div>}
      </div>

      <div className="banner gate">
        <span>◌</span>
        <span>
          {app.t(
            "Agent list/meta/history, permission preview and the sandbox panel land with their faces (roadmap §7.5).",
            "Agent 列表/历史、权限预览与沙箱面板随对应端点落地(路线 §7.5)。",
          )}
        </span>
      </div>
    </>
  );
}
