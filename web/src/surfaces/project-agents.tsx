// Project · Agents: per-(project, agent) MCP binding. Reference-don't-copy —
// the checklist lists workspace supply; saving fails closed on unknown ids.

import { useMutation, useQuery } from "@tanstack/react-query";
import { useEffect, useState } from "react";
import { useParams } from "react-router";
import { api, isAbsent } from "../lib/api/client";
import type { McpServerDef, ProjectAgentConfig, ResolvedMcpServerView } from "../lib/api/types";
import { useApp } from "../lib/app-state";

const WORKSPACE = "wrkspc_default";

export default function ProjectAgentsSurface() {
  const app = useApp();
  const { pid = "" } = useParams();
  const [agentId, setAgentId] = useState("default");
  const [selected, setSelected] = useState<string[]>([]);
  const servers = useQuery({
    queryKey: ["mcp-servers"],
    queryFn: () => api.get<McpServerDef[]>("/v1/config/mcp-servers"),
  });
  const binding = useQuery({
    queryKey: ["project-agent-mcp", pid, agentId],
    queryFn: () => api.get<ProjectAgentConfig>(`/v1/config/projects/${pid}/agents/${agentId}/mcp`),
    retry: false,
    enabled: !!pid && !!agentId,
  });
  useEffect(() => {
    if (binding.data) setSelected(binding.data.mcp_server_ids);
    else if (binding.isError && isAbsent(binding.error)) setSelected([]);
  }, [binding.data, binding.isError, binding.error]);

  const save = useMutation({
    mutationFn: () =>
      api.put<ProjectAgentConfig>(`/v1/config/projects/${pid}/agents/${agentId}/mcp`, {
        project_id: pid,
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
      <div className="card">
        <h2>{app.t("Per-project MCP binding", "项目级 MCP 绑定")}</h2>
        <p className="hint">
          {app.t(
            "A project SELECTS from workspace supply — pick which authored MCP servers this agent uses inside this project. Unknown ids fail closed.",
            "项目从工作区供给中「选择」——勾选该 agent 在本项目内可用的已作者化 MCP 服务器;未知 id 会被拒绝。",
          )}
        </p>
        <div className="row" style={{ marginBottom: 12 }}>
          <span className="field">
            <label>Agent id</label>
            <input className="input mono" value={agentId} onChange={(e) => setAgentId(e.target.value)} />
          </span>
          <span style={{ flex: 1 }} />
          <button className="btn ghost" onClick={() => resolve.mutate()}>
            {app.t("Resolve preview (workspace binding)", "Resolve 预览(工作区绑定)")}
          </button>
          <button className="btn primary" disabled={save.isPending} onClick={() => save.mutate()}>
            {app.t("Save binding", "保存绑定")}
          </button>
        </div>
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
            <span>{s.display_name}</span>
            <span className="mono mut" style={{ marginLeft: "auto", fontSize: 11 }}>
              {s.url}
            </span>
          </label>
        ))}
        {(servers.data ?? []).length === 0 && (
          <div className="mut">{app.t("No MCP servers authored — add them in Workspace · MCP servers.", "尚无 MCP 服务器——到工作区 · MCP 页添加。")}</div>
        )}
        {save.error instanceof Error && <div className="err">{save.error.message}</div>}
        {binding.data && (
          <div className="mut" style={{ marginTop: 8 }}>
            {app.t("Current binding v", "当前绑定 v")}
            {binding.data.version}: {binding.data.mcp_server_ids.join(", ") || "(empty)"}
          </div>
        )}
      </div>
      {resolve.data && (
        <div className="card">
          <h2>Resolved</h2>
          <div className="chain">
            {resolve.data.map((r) => (
              <span key={r.url} className="chip">
                {r.name}
                <span className="mono mut">{r.url}</span>
                <span className={`dot`} style={{ background: r.credential_present ? "var(--ok)" : "var(--fg3)" }} />
              </span>
            ))}
            {resolve.data.length === 0 && <span className="mut">(empty)</span>}
          </div>
        </div>
      )}
      {resolve.error instanceof Error && <div className="err">{resolve.error.message}</div>}
    </>
  );
}
