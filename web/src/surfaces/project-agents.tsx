// Project · Agents: the agent is a project-container resource (Managed Agents
// wire). This page carries the two operations that exist today — config
// draft→validate→publish, and the per-project MCP binding (reference-don't-copy
// over workspace supply). Full agent CRUD/versioning moves onto the managed
// wire under /projects/{pid}/v1/agents (design/web-ui.md §7); the config plane
// endpoints below are the interim, admin-plane home.

import { useMutation, useQuery } from "@tanstack/react-query";
import { useEffect, useState } from "react";
import { useParams } from "react-router";
import { api, isAbsent } from "../lib/api/client";
import type {
  McpServerDef,
  ProjectAgentConfig,
  ResolvedMcpServerView,
} from "../lib/api/types";
import { useApp } from "../lib/app-state";

const WORKSPACE = "wrkspc_default";

// Option B: MCP is declared INLINE on the agent — {type,name,url}, no auth.
// The runtime credential comes from the project Vault, matched by url.
const DRAFT_TEMPLATE = `{
  "instructions": "You are a helpful coding agent.",
  "model": "claude-sonnet-4-5",
  "mcp_servers": [
    { "type": "url", "name": "docs", "url": "https://mcp.example.com/docs" }
  ]
}`;

export default function ProjectAgentsSurface() {
  const app = useApp();
  const { pid = "" } = useParams();
  const [agentId, setAgentId] = useState("default");

  // --- config: draft → validate → publish (admin plane; project-scoping is §7) ---
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

  // --- per-project MCP binding (genuinely project-scoped, ScopeRef::Project target) ---
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
  const [selected, setSelected] = useState<string[]>([]);
  useEffect(() => {
    if (binding.data) setSelected(binding.data.mcp_server_ids);
    else if (binding.isError && isAbsent(binding.error)) setSelected([]);
  }, [binding.data, binding.isError, binding.error]);

  const saveBinding = useMutation({
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
      <p className="mut" style={{ margin: 0 }}>
        {app.t(
          "Agents are project resources. MCP servers are declared inline (url only); their runtime credentials come from the project Vault. The agent references only the model by id from workspace supply.",
          "Agent 是项目资源。MCP 服务器内联声明(仅 url),运行凭证来自 Project Vault;agent 仅按 id 引用工作区供给里的模型。",
        )}
      </p>
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
        <h2>{app.t("Per-project MCP binding (optional catalog path)", "项目级 MCP 绑定(可选目录路径)")}</h2>
        <p className="hint">
          {app.t(
            "Optional: instead of inlining above, reference the shared MCP catalog by id (central reuse/governance). The project SELECTS from workspace supply; unknown ids fail closed.",
            "可选:不在上方内联,而是按 id 引用共享 MCP 目录(集中复用/治理)。项目从工作区供给中「选择」;未知 id 会被拒绝。",
          )}
        </p>
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
          <div className="mut">
            {app.t("No MCP servers authored — add them in Workspace · MCP servers.", "尚无 MCP 服务器——到工作区 · MCP 页添加。")}
          </div>
        )}
        <div className="row" style={{ marginTop: 8 }}>
          <button className="btn primary" disabled={saveBinding.isPending} onClick={() => saveBinding.mutate()}>
            {app.t("Save binding", "保存绑定")}
          </button>
          <button className="btn ghost" onClick={() => resolve.mutate()}>
            {app.t("Resolve preview", "Resolve 预览")}
          </button>
          {binding.data && (
            <span className="mut">
              {app.t("v", "v")}
              {binding.data.version}
            </span>
          )}
        </div>
        {saveBinding.error instanceof Error && <div className="err">{saveBinding.error.message}</div>}
        {resolve.data && (
          <div className="chain" style={{ marginTop: 8 }}>
            {resolve.data.map((r) => (
              <span key={r.url} className="chip">
                {r.name}
                <span className="mono mut">{r.url}</span>
                <span className="dot" style={{ background: r.credential_present ? "var(--ok)" : "var(--fg3)" }} />
              </span>
            ))}
            {resolve.data.length === 0 && <span className="mut">(empty)</span>}
          </div>
        )}
        {resolve.error instanceof Error && <div className="err">{resolve.error.message}</div>}
      </div>

      <div className="banner gate">
        <span>◌</span>
        <span>
          {app.t(
            "Full agent CRUD + versioning move onto the Managed Agents wire under /projects/{pid}/v1/agents (roadmap §7); list/meta/history and the sandbox panel land with their faces.",
            "完整的 agent CRUD + 版本化迁移到 /projects/{pid}/v1/agents 的 Managed Agents wire(路线 §7);列表/历史与沙箱面板随对应端点落地。",
          )}
        </span>
      </div>
    </>
  );
}
