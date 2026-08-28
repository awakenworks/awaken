// Workspace · Sessions: the scoped session list (GET /v1/sessions,
// tenant-scoped via ws()), Anthropic-console style — mono ids, status pills, one
// primary action. Tenancy fences the list by the active workspace (ADR-0051);
// archive marks a row without removing it.

import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import {
  isManagedSessionActiveStatus,
  managedSessionStatusPresentation,
} from "@awaken/managed-session-projection";
import { useRef, useState } from "react";
import { useNavigate, useParams } from "react-router";
import Drawer from "../components/ui/Drawer";
import { Button, Card, Modal, Pill, Segmented, TextField, useConfirm, useToast } from "../components/ui";
import {
  BUILTIN_LOCAL_ENVIRONMENT_ID,
  api,
  createManagedSession,
  IdempotencyScope,
  ws,
} from "../lib/api/client";
import type {
  AgentConfigList,
  CreateSessionRequest,
  Environment,
  ListSessionsResponse,
  Page,
  Session,
  Vault,
} from "../lib/api/types";
import { useApp } from "../lib/app-state";
import { visibleAgents } from "../lib/visible-agents";
import EnvironmentsSurface from "./environments";
import AgentsSurface from "./agents";

export function StatusPill({ session }: { session: Session }) {
  const app = useApp();
  if (session.archived_at) {
    return <Pill tone="neutral">{app.t("archived", "已归档")}</Pill>;
  }
  const status = managedSessionStatusPresentation(session.status);
  if (status === "running") {
    return (
      <span className="pill agent">
        <span className="dot pulse" style={{ background: "var(--agent)" }} />
        {app.t("running", "运行中")}
      </span>
    );
  }
  if (status === "idle") {
    return <Pill tone="ok">{app.t("idle", "空闲")}</Pill>;
  }
  if (status === "rescheduling") {
    return <Pill tone="info">{app.t("rescheduling", "重新调度中")}</Pill>;
  }
  if (status === "terminated") {
    return <Pill tone="neutral">{app.t("terminated", "已终止")}</Pill>;
  }
  return <Pill tone="warn">{app.t("unknown", "状态未知")}</Pill>;
}

function NewSessionModal({ wsId, onClose }: { wsId: string; onClose: () => void }) {
  const app = useApp();
  const nav = useNavigate();
  const [agent, setAgent] = useState("");
  const [environmentId, setEnvironmentId] = useState(BUILTIN_LOCAL_ENVIRONMENT_ID);
  const [title, setTitle] = useState("");
  const [vaultIds, setVaultIds] = useState<string[]>([]);
  const [mcp, setMcp] = useState<{ name: string; url: string }[]>([]);
  const [manage, setManage] = useState<"agents" | "environments" | null>(null);
  const createIdentity = useRef(new IdempotencyScope("console-session-create"));
  // Inline pickers over the config plane (published agents) + environments.
  const agents = useQuery({
    queryKey: ["config-agents", wsId],
    queryFn: () => api.get<AgentConfigList>(ws("/v1/config/agents")),
  });
  const envs = useQuery({
    queryKey: ["environments", wsId],
    queryFn: () => api.get<Page<Environment>>(ws("/v1/environments")),
  });
  const vaults = useQuery({
    queryKey: ["vaults", wsId],
    queryFn: () => api.get<Page<Vault>>(ws("/v1/vaults")),
  });
  const create = useMutation({
    mutationFn: (body: CreateSessionRequest) =>
      createManagedSession(body, createIdentity.current),
    onSuccess: (session) => {
      createIdentity.current.complete();
      nav(`/w/${wsId}/sessions/${session.id}`);
    },
  });
  const selectedEnvironment = (envs.data?.data ?? []).find((environment) => environment.id === environmentId);
  const selectedEnvironmentHasPackages = selectedEnvironment?.config.packages
    && Object.entries(selectedEnvironment.config.packages)
      .some(([key, packages]) => key !== "type" && Array.isArray(packages) && packages.length > 0);
  return (
    <>
      <Modal title={<>{app.t("New session", "新建会话")} · {wsId}</>} onClose={onClose}>
        <div className="field">
          <label className="row" style={{ justifyContent: "space-between" }}>
            <span>Agent</span>
            <button className="manage-link" onClick={() => setManage("agents")}>
              {app.t("Manage ↗", "管理 ↗")}
            </button>
          </label>
          <select className="input mono" aria-label="Agent" value={agent} onChange={(e) => setAgent(e.target.value)}>
            <option value="">{app.t("Select a published Agent…", "选择已发布的 Agent…")}</option>
            {visibleAgents(agents.data?.data).filter((a) => a.published).map((a) => (
              <option key={a.id} value={a.id}>
                {a.name || a.id}{a.name ? ` · ${a.id}` : ""}
              </option>
            ))}
          </select>
          {!agents.isLoading && visibleAgents(agents.data?.data).filter((a) => a.published).length === 0 && (
            <div className="banner warn"><span>→</span><span>{app.t("No published Agent is available. Create and publish an Agent before starting a Session.", "没有可用的已发布 Agent。请先创建并发布 Agent，再启动会话。")}</span></div>
          )}
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
            aria-label={app.t("Environment", "运行环境")}
            value={environmentId}
            onChange={(e) => setEnvironmentId(e.target.value)}
          >
            <option value={BUILTIN_LOCAL_ENVIRONMENT_ID}>{app.t("Local Environment", "本地运行环境")}</option>
            {(envs.data?.data ?? []).filter((environment) => (
              !environment.archived_at && environment.id !== BUILTIN_LOCAL_ENVIRONMENT_ID
            )).map((e) => (
              <option key={e.id} value={e.id}>
                {e.name} · {e.id}
              </option>
            ))}
          </select>
          {selectedEnvironmentHasPackages && <div className="banner info"><span>ⓘ</span><span>{app.t(
            "This Environment requires package installation. Use it only with a package-capable Environment provider.",
            "此 Environment 需要安装 Package；请仅在具备 Package 安装能力的 Environment Provider 上使用。",
          )}</span></div>}
        </div>
        <TextField
          label={app.t("Session title · optional", "会话标题 · 可选")}
          hint={app.t("Use a task or customer reference that will make this run easy to find later.", "填写任务或客户标识，便于之后找到这次运行。")}
          value={title}
          onChange={(e) => setTitle(e.target.value)}
        />
        <details>
          <summary>{app.t("Advanced runtime overrides", "高级运行覆盖")}</summary>
          <p className="mut">{app.t("Usually leave these empty. The Agent's published MCP and resource configuration is included automatically.", "通常无需设置。Agent 已发布的 MCP 与资源配置会自动包含。")}</p>
          <div className="field">
            <label>{app.t("Runtime Secret Vaults for this Session", "本次会话使用的运行时凭证 Vault")}</label>
            <span className="mut">{app.t("Select only Vaults whose tools or processes are needed for this run.", "只选择本次运行所需工具或进程对应的 Vault。")}</span>
            <div className="check-picker">
              {(vaults.data?.data ?? []).filter((vault) => !vault.archived_at).map((vault) => (
                <label key={vault.id} className="check-row">
                  <input type="checkbox" checked={vaultIds.includes(vault.id)} onChange={(event) => setVaultIds(event.target.checked ? [...vaultIds, vault.id] : vaultIds.filter((id) => id !== vault.id))} />
                  <span style={{ display: "flex", flexDirection: "column" }}>{vault.display_name || app.t("Unnamed Vault", "未命名 Vault")}<small className="mono mut">{vault.id}</small></span>
                </label>
              ))}
              {!vaults.isLoading && (vaults.data?.data ?? []).filter((vault) => !vault.archived_at).length === 0 && <span className="mut">{app.t("No Runtime Secret Vaults are available.", "没有可用的运行时凭证 Vault。")}</span>}
            </div>
          </div>
          <div className="field">
          <label>{app.t("Temporary MCP servers for this Session", "仅用于本次会话的临时 MCP 服务器")}</label>
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
            + {app.t("Add temporary server", "添加临时服务器")}
          </Button>
          <span className="mut">{app.t("A non-empty list is an official one-session replacement of the Agent's MCP servers. Add reusable servers to the Agent instead.", "非空列表会按官方协议在本次 Session 中替换 Agent 的 MCP 服务器。可复用服务器应添加到 Agent。")}</span>
          </div>
        </details>
        {create.error instanceof Error && <div className="err">{create.error.message}</div>}
        <div className="row" style={{ justifyContent: "flex-end" }}>
          <Button onClick={onClose}>
            {app.t("Cancel", "取消")}
          </Button>
          <Button
            variant="primary"
            disabled={create.isPending || !agent}
            onClick={() => {
              const mcpServers = mcp
                .filter((server) => server.name && server.url)
                .map((server) => ({ type: "url" as const, ...server }));
              create.mutate({
                agent: mcpServers.length > 0
                  ? { id: agent, type: "agent_with_overrides", mcp_servers: mcpServers }
                  : agent,
                environment_id: environmentId,
                title: title || undefined,
                vault_ids: vaultIds,
              });
            }}
          >
            {app.t("Create", "创建")} ➤
          </Button>
        </div>
      </Modal>
      {manage === "agents" && (
        <Drawer title={app.t("Agents", "Agents")} onClose={() => setManage(null)}>
          <AgentsSurface />
        </Drawer>
      )}
      {manage === "environments" && (
        <Drawer title={app.t("Environments", "运行环境")} onClose={() => setManage(null)}>
          <EnvironmentsSurface />
        </Drawer>
      )}
    </>
  );
}

export default function SessionsSurface() {
  const app = useApp();
  const nav = useNavigate();
  const qc = useQueryClient();
  const confirm = useConfirm();
  const toast = useToast();
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
    onSuccess: () => {
      void qc.invalidateQueries({ queryKey: ["sessions", wsId] });
      toast.ok(app.t("Session archived.", "会话已归档。"));
    },
    onError: (cause) => toast.err(cause instanceof Error ? cause.message : String(cause)),
  });
  const archiveSession = async (sid: string) => {
    const approved = await confirm({
      title: app.t("Archive this session?", "归档该会话？"),
      body: app.t("It remains readable, but no new work should be sent to it.", "它仍可读取，但不应再向其发送新任务。"),
      confirmLabel: app.t("Archive", "归档"),
    });
    if (approved) archive.mutate(sid);
  };
  const rows = (sessions.data?.data ?? []).filter((s) =>
    filter === "all"
      ? true
      : filter === "archived"
        ? !!s.archived_at
        : isManagedSessionActiveStatus(s.status) && !s.archived_at,
  );

  return (
    <>
      <div className="row" style={{ justifyContent: "space-between" }}>
        <Segmented
            value={filter}
            onChange={setFilter}
            options={[
              { value: "all", label: app.t("All", "全部") },
              { value: "running", label: app.t("Running", "运行中") },
              { value: "archived", label: app.t("Archived", "已归档") },
            ]}
          />
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
                      disabled={archive.isPending && archive.variables === s.id}
                      onClick={(e) => {
                        e.stopPropagation();
                        void archiveSession(s.id);
                      }}
                    >
                      {archive.isPending && archive.variables === s.id ? app.t("Archiving…", "正在归档…") : app.t("Archive", "归档")}
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
      <details>
        <summary>{app.t("Open a Session by ID", "按 ID 打开会话")}</summary>
        <div className="row" style={{ marginTop: 8 }}>
          <input className="input mono" aria-label={app.t("Session ID", "会话 ID")} style={{ width: 320 }} placeholder="sesn_…" value={openId} onChange={(e) => setOpenId(e.target.value)} />
          <Button disabled={!openId.trim()} onClick={() => nav(`/w/${wsId}/sessions/${openId.trim()}`)}>{app.t("Open Session", "打开会话")}</Button>
        </div>
      </details>
      {creating && <NewSessionModal wsId={wsId} onClose={() => setCreating(false)} />}
    </>
  );
}
