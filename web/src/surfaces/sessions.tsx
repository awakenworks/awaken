// Project · Sessions: the project container's session list (GET
// /projects/{pid}/v1/sessions), Anthropic-console style — mono ids, status
// pills, one primary action. "Needs you" filters on the server's derived
// status; archive marks a row without removing it.

import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useState } from "react";
import { useNavigate, useParams } from "react-router";
import { api } from "../lib/api/client";
import type { CreateSessionRequest, ListSessionsResponse, Session } from "../lib/api/types";
import { useApp } from "../lib/app-state";

export function StatusPill({ session }: { session: Session }) {
  const app = useApp();
  if (session.archived_at) {
    return <span className="pill neutral">{app.t("archived", "已归档")}</span>;
  }
  if (session.status === "running") {
    return (
      <span className="pill agent">
        <span className="dot pulse" style={{ background: "var(--agent)" }} />
        {app.t("running", "运行中")}
      </span>
    );
  }
  return <span className="pill ok">idle</span>;
}

function NewSessionModal({ pid, onClose }: { pid: string; onClose: () => void }) {
  const app = useApp();
  const nav = useNavigate();
  const [agent, setAgent] = useState("default");
  const [title, setTitle] = useState("");
  const [vaultIds, setVaultIds] = useState("");
  const [mcp, setMcp] = useState<{ name: string; url: string }[]>([]);
  const create = useMutation({
    mutationFn: (body: CreateSessionRequest) =>
      api.post<Session>(`/projects/${pid}/v1/sessions`, body),
    onSuccess: (session) => {
      nav(`/p/${pid}/sessions/${session.id}`);
    },
  });
  return (
    <div className="overlay" onClick={onClose}>
      <div className="modal" onClick={(e) => e.stopPropagation()}>
        <h3>
          {app.t("New session", "新建会话")} · {pid}
        </h3>
        <div className="field">
          <label>Agent</label>
          <input className="input mono" value={agent} onChange={(e) => setAgent(e.target.value)} />
        </div>
        <div className="field">
          <label>{app.t("Title", "标题")}</label>
          <input className="input" value={title} onChange={(e) => setTitle(e.target.value)} />
        </div>
        <div className="field">
          <label>{app.t("Vault ids (comma separated)", "Vault id(逗号分隔)")}</label>
          <input
            className="input mono"
            placeholder="vlt_…"
            value={vaultIds}
            onChange={(e) => setVaultIds(e.target.value)}
          />
        </div>
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
              <button className="btn ghost" onClick={() => setMcp(mcp.filter((_, j) => j !== i))}>
                ✕
              </button>
            </div>
          ))}
          <button className="btn ghost" onClick={() => setMcp([...mcp, { name: "", url: "" }])}>
            + {app.t("add inline server", "添加内联服务器")}
          </button>
          <span className="mut">{app.t("Project-bound MCP servers merge in automatically.", "项目绑定的 MCP 自动并入。")}</span>
        </div>
        {create.error instanceof Error && <div className="err">{create.error.message}</div>}
        <div className="row" style={{ justifyContent: "flex-end" }}>
          <button className="btn" onClick={onClose}>
            {app.t("Cancel", "取消")}
          </button>
          <button
            className="btn primary"
            disabled={create.isPending}
            onClick={() =>
              create.mutate({
                agent,
                title: title || undefined,
                vault_ids: vaultIds
                  .split(",")
                  .map((s) => s.trim())
                  .filter(Boolean),
                mcp_servers: mcp.filter((m) => m.name && m.url),
              })
            }
          >
            {app.t("Create", "创建")} ➤
          </button>
        </div>
      </div>
    </div>
  );
}

export default function SessionsSurface() {
  const app = useApp();
  const nav = useNavigate();
  const qc = useQueryClient();
  const { pid = "" } = useParams();
  const [creating, setCreating] = useState(false);
  const [openId, setOpenId] = useState("");
  const [needsYou, setNeedsYou] = useState(false);

  const sessions = useQuery({
    queryKey: ["sessions", pid, needsYou],
    queryFn: () =>
      api.get<ListSessionsResponse>(
        `/projects/${pid}/v1/sessions${needsYou ? "?status=requires_action" : ""}`,
      ),
    refetchInterval: 15_000,
  });
  const archive = useMutation({
    mutationFn: (sid: string) => api.post<Session>(`/projects/${pid}/v1/sessions/${sid}/archive`),
    onSuccess: () => void qc.invalidateQueries({ queryKey: ["sessions", pid] }),
  });
  const rows = sessions.data?.data ?? [];

  return (
    <>
      <div className="row" style={{ justifyContent: "space-between" }}>
        <span className="row">
          <button
            className={`btn ${needsYou ? "" : "primary"}`}
            style={{ height: 26 }}
            onClick={() => setNeedsYou(false)}
          >
            {app.t("All", "全部")}
          </button>
          <button
            className={`btn ${needsYou ? "primary" : ""}`}
            style={{ height: 26 }}
            onClick={() => setNeedsYou(true)}
          >
            ⚠ {app.t("Awaiting action", "待客户端动作")}
          </button>
          <span className="mut">
            baseURL <code>/projects/{pid}</code>
            <button
              className="btn ghost"
              style={{ height: 22, marginLeft: 6 }}
              onClick={() => navigator.clipboard.writeText(`${location.origin}/projects/${pid}`)}
            >
              copy
            </button>
          </span>
        </span>
        <button className="btn primary" onClick={() => setCreating(true)}>
          + {app.t("New session", "新建会话")}
        </button>
      </div>
      {sessions.error instanceof Error && <div className="err">{sessions.error.message}</div>}
      <div className="card" style={{ padding: 0 }}>
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
              <tr key={s.id} data-click="true" onClick={() => nav(`/p/${pid}/sessions/${s.id}`)}>
                <td className="mono">{s.id}</td>
                <td>{s.title || <span className="mut">(untitled)</span>}</td>
                <td>
                  <span className="pill agent">{s.agent.id}</span>
                </td>
                <td>
                  <StatusPill session={s} />
                </td>
                <td style={{ textAlign: "right" }}>
                  {!s.archived_at && (
                    <button
                      className="btn ghost"
                      style={{ height: 22 }}
                      disabled={archive.isPending}
                      onClick={(e) => {
                        e.stopPropagation();
                        archive.mutate(s.id);
                      }}
                    >
                      {app.t("Archive", "归档")}
                    </button>
                  )}
                </td>
              </tr>
            ))}
            {rows.length === 0 && (
              <tr>
                <td colSpan={5} className="mut">
                  {sessions.isLoading
                    ? "…"
                    : needsYou
                      ? app.t("Nothing awaiting action.", "没有等待客户端动作的会话。")
                      : app.t("No sessions yet.", "还没有会话。")}
                </td>
              </tr>
            )}
          </tbody>
        </table>
      </div>
      <div className="row">
        <input
          className="input mono"
          style={{ width: 320 }}
          placeholder="sesn_…"
          value={openId}
          onChange={(e) => setOpenId(e.target.value)}
        />
        <button
          className="btn"
          disabled={!openId.trim()}
          onClick={() => nav(`/p/${pid}/sessions/${openId.trim()}`)}
        >
          {app.t("Open by id", "按 id 打开")}
        </button>
      </div>
      {creating && <NewSessionModal pid={pid} onClose={() => setCreating(false)} />}
    </>
  );
}
