// Project · Sessions: the workspace-scoped session list (GET /v1/sessions,
// tenant-scoped via ws()), Anthropic-console style — mono ids, status pills, one
// primary action. Tenancy fences the list by the active workspace (ADR-0051);
// archive marks a row without removing it.

import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useState } from "react";
import { useNavigate, useParams } from "react-router";
import Drawer from "../components/ui/Drawer";
import { Button, Card, Pill, Segmented, TextField } from "../components/ui";
import { api, getWorkspace, ws } from "../lib/api/client";
import type {
  AgentConfigList,
  CreateSessionRequest,
  Environment,
  ListSessionsResponse,
  Page,
  Session,
} from "../lib/api/types";
import { useApp } from "../lib/app-state";
import EnvironmentsSurface from "./environments";
import ProjectAgentsSurface from "./project-agents";

export function StatusPill({ session }: { session: Session }) {
  const app = useApp();
  if (session.archived_at) {
    return <Pill tone="neutral">{app.t("archived", "已归档")}</Pill>;
  }
  if (session.status === "running") {
    return (
      <span className="pill agent">
        <span className="dot pulse" style={{ background: "var(--agent)" }} />
        {app.t("running", "运行中")}
      </span>
    );
  }
  return <Pill tone="ok">idle</Pill>;
}

function NewSessionModal({ wsId, onClose }: { wsId: string; onClose: () => void }) {
  const app = useApp();
  const nav = useNavigate();
  const [agent, setAgent] = useState("");
  const [environmentId, setEnvironmentId] = useState("");
  const [title, setTitle] = useState("");
  const [vaultIds, setVaultIds] = useState("");
  const [mcp, setMcp] = useState<{ name: string; url: string }[]>([]);
  const [manage, setManage] = useState<"agents" | "environments" | null>(null);
  // Inline pickers over the config plane (published agents) + environments.
  const agents = useQuery({
    queryKey: ["config-agents"],
    queryFn: () => api.get<AgentConfigList>("/v1/config/agents"),
  });
  const envs = useQuery({
    queryKey: ["environments"],
    queryFn: () => api.get<Page<Environment>>("/v1/environments"),
  });
  const create = useMutation({
    mutationFn: (body: CreateSessionRequest) =>
      api.post<Session>(ws("/v1/sessions"), body),
    onSuccess: (session) => {
      nav(`/w/${wsId}/sessions/${session.id}`);
    },
  });
  return (
    <div className="overlay" onClick={onClose}>
      <div className="modal" onClick={(e) => e.stopPropagation()}>
        <h3>
          {app.t("New session", "新建会话")} · {wsId}
        </h3>
        <div className="field">
          <label className="row" style={{ justifyContent: "space-between" }}>
            <span>Agent</span>
            <button className="manage-link" onClick={() => setManage("agents")}>
              {app.t("Manage ↗", "管理 ↗")}
            </button>
          </label>
          <select className="input mono" value={agent} onChange={(e) => setAgent(e.target.value)}>
            <option value="">{app.t("— select an agent —", "— 选择 agent —")}</option>
            {(agents.data?.data ?? []).map((a) => (
              <option key={a.id} value={a.id}>
                {a.id}
                {a.published ? "" : app.t(" (draft)", "(草稿)")}
              </option>
            ))}
          </select>
        </div>
        <div className="field">
          <label className="row" style={{ justifyContent: "space-between" }}>
            <span>{app.t("Environment", "运行环境")}</span>
            <button className="manage-link" onClick={() => setManage("environments")}>
              {app.t("Manage ↗", "管理 ↗")}
            </button>
          </label>
          <select
            className="input mono"
            value={environmentId}
            onChange={(e) => setEnvironmentId(e.target.value)}
          >
            <option value="">{app.t("— default —", "— 默认 —")}</option>
            {(envs.data?.data ?? []).map((e) => (
              <option key={e.id} value={e.id}>
                {e.name} · {e.id}
              </option>
            ))}
          </select>
        </div>
        <TextField
          label={app.t("Title", "标题")}
          value={title}
          onChange={(e) => setTitle(e.target.value)}
        />
        <TextField
          label={app.t("Vault ids (comma separated)", "Vault id(逗号分隔)")}
          mono
          placeholder="vlt_…"
          value={vaultIds}
          onChange={(e) => setVaultIds(e.target.value)}
        />
        <div className="field">
          <label>{app.t("Inline MCP servers", "内联 MCP 服务器")}</label>
          {mcp.map((m, i) => (
            <div className="row" key={i}>
              <input
                className="input"
                placeholder="name"
                value={m.name}
                onChange={(e) => setMcp(mcp.map((x, j) => (j === i ? { ...x, name: e.target.value } : x)))}
              />
              <input
                className="input mono"
                style={{ flex: 1 }}
                placeholder="https://…"
                value={m.url}
                onChange={(e) => setMcp(mcp.map((x, j) => (j === i ? { ...x, url: e.target.value } : x)))}
              />
              <Button variant="ghost" onClick={() => setMcp(mcp.filter((_, j) => j !== i))}>
                ✕
              </Button>
            </div>
          ))}
          <Button variant="ghost" onClick={() => setMcp([...mcp, { name: "", url: "" }])}>
            + {app.t("add inline server", "添加内联服务器")}
          </Button>
          <span className="mut">{app.t("Project-bound MCP servers merge in automatically.", "项目绑定的 MCP 自动并入。")}</span>
        </div>
        {create.error instanceof Error && <div className="err">{create.error.message}</div>}
        <div className="row" style={{ justifyContent: "flex-end" }}>
          <Button onClick={onClose}>
            {app.t("Cancel", "取消")}
          </Button>
          <Button
            variant="primary"
            disabled={create.isPending || !agent}
            onClick={() => {
              // The environment carries the runtime; the session declares it to the host
              // via the `awaken.runtime` metadata key (protocol-managed ext::model_selection).
              // So "pick a claude-sandbox environment" is how a user selects an ACP runtime —
              // no runtime field leaks onto the (protocol-neutral) agent.
              const env = (envs.data?.data ?? []).find((e) => e.id === environmentId);
              const runtime = env?.config.runtime;
              create.mutate({
                agent,
                environment_id: environmentId || undefined,
                title: title || undefined,
                ...(runtime ? { metadata: { "awaken.runtime": runtime } } : {}),
                vault_ids: vaultIds
                  .split(",")
                  .map((s) => s.trim())
                  .filter(Boolean),
                mcp_servers: mcp.filter((m) => m.name && m.url),
              });
            }}
          >
            {app.t("Create", "创建")} ➤
          </Button>
        </div>
      </div>
      {manage === "agents" && (
        <Drawer title={app.t("Agents", "Agents")} onClose={() => setManage(null)}>
          <ProjectAgentsSurface />
        </Drawer>
      )}
      {manage === "environments" && (
        <Drawer title={app.t("Environments", "运行环境")} onClose={() => setManage(null)}>
          <EnvironmentsSurface />
        </Drawer>
      )}
    </div>
  );
}

export default function SessionsSurface() {
  const app = useApp();
  const nav = useNavigate();
  const qc = useQueryClient();
  const { ws: wsId = "default" } = useParams();
  const [creating, setCreating] = useState(false);
  const [openId, setOpenId] = useState("");
  // Anthropic's ListSessions is not server-filtered; scope by status client-side.
  const [filter, setFilter] = useState<"all" | "running" | "archived">("all");

  const sessions = useQuery({
    queryKey: ["sessions", wsId],
    queryFn: () => api.get<ListSessionsResponse>(ws("/v1/sessions")),
    refetchInterval: 15_000,
  });
  const archive = useMutation({
    mutationFn: (sid: string) => api.post<Session>(ws(`/v1/sessions/${sid}/archive`)),
    onSuccess: () => void qc.invalidateQueries({ queryKey: ["sessions", wsId] }),
  });
  const rows = (sessions.data?.data ?? []).filter((s) =>
    filter === "all"
      ? true
      : filter === "archived"
        ? !!s.archived_at
        : s.status === "running" && !s.archived_at,
  );

  return (
    <>
      <div className="row" style={{ justifyContent: "space-between" }}>
        <span className="row">
          <Segmented
            value={filter}
            onChange={setFilter}
            options={[
              { value: "all", label: app.t("All", "全部") },
              { value: "running", label: app.t("Running", "运行中") },
              { value: "archived", label: app.t("Archived", "已归档") },
            ]}
          />
          <span className="mut">
            baseURL <code>{getWorkspace() ? `/v1/workspaces/${getWorkspace()}` : "/ (default scope)"}</code>
            <Button
              variant="ghost"
              style={{ height: 22, marginLeft: 6 }}
              onClick={() =>
                navigator.clipboard.writeText(
                  getWorkspace() ? `${location.origin}/v1/workspaces/${getWorkspace()}` : location.origin,
                )
              }
            >
              copy
            </Button>
          </span>
        </span>
        <Button variant="primary" onClick={() => setCreating(true)}>
          + {app.t("New session", "新建会话")}
        </Button>
      </div>
      {sessions.error instanceof Error && <div className="err">{sessions.error.message}</div>}
      <Card style={{ padding: 0 }}>
        <table className="table">
          <thead>
            <tr>
              <th>Session</th>
              <th>{app.t("Title", "标题")}</th>
              <th>Agent</th>
              <th>{app.t("Status", "状态")}</th>
              <th />
            </tr>
          </thead>
          <tbody>
            {rows.map((s) => (
              <tr key={s.id} data-click="true" onClick={() => nav(`/w/${wsId}/sessions/${s.id}`)}>
                <td className="mono">{s.id}</td>
                <td>{s.title || <span className="mut">(untitled)</span>}</td>
                <td>
                  <Pill tone="agent">{s.agent.id}</Pill>
                </td>
                <td>
                  <StatusPill session={s} />
                </td>
                <td style={{ textAlign: "right" }}>
                  {!s.archived_at && (
                    <Button
                      variant="ghost"
                      style={{ height: 22 }}
                      disabled={archive.isPending}
                      onClick={(e) => {
                        e.stopPropagation();
                        archive.mutate(s.id);
                      }}
                    >
                      {app.t("Archive", "归档")}
                    </Button>
                  )}
                </td>
              </tr>
            ))}
            {rows.length === 0 && (
              <tr>
                <td colSpan={5} className="mut">
                  {sessions.isLoading
                    ? "…"
                    : filter === "all"
                      ? app.t("No sessions yet.", "还没有会话。")
                      : app.t("None in this state.", "该状态下没有会话。")}
                </td>
              </tr>
            )}
          </tbody>
        </table>
      </Card>
      <div className="row">
        <input
          className="input mono"
          style={{ width: 320 }}
          placeholder="sesn_…"
          value={openId}
          onChange={(e) => setOpenId(e.target.value)}
        />
        <Button
          disabled={!openId.trim()}
          onClick={() => nav(`/w/${wsId}/sessions/${openId.trim()}`)}
        >
          {app.t("Open by id", "按 id 打开")}
        </Button>
      </div>
      {creating && <NewSessionModal wsId={wsId} onClose={() => setCreating(false)} />}
    </>
  );
}
