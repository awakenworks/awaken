// Project · Sessions. There is no list endpoint yet (design/web-ui.md §7.1),
// so the list is a local registry of sessions created/opened in this browser,
// plus open-by-id — honest about the gap while the create/detail flow is real.

import { useMutation } from "@tanstack/react-query";
import { useState } from "react";
import { useNavigate, useParams } from "react-router";
import { api } from "../lib/api/client";
import type { CreateSessionRequest, Session } from "../lib/api/types";
import { useApp } from "../lib/app-state";

export function sessionRegistry(pid: string): { id: string; title: string }[] {
  try {
    return JSON.parse(localStorage.getItem(`awaken.console.sessions.${pid}`) ?? "[]") as {
      id: string;
      title: string;
    }[];
  } catch {
    return [];
  }
}
export function rememberSession(pid: string, id: string, title: string) {
  const list = sessionRegistry(pid).filter((s) => s.id !== id);
  list.unshift({ id, title });
  localStorage.setItem(`awaken.console.sessions.${pid}`, JSON.stringify(list.slice(0, 50)));
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
      rememberSession(pid, session.id, session.title ?? "");
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
  const { pid = "" } = useParams();
  const [creating, setCreating] = useState(false);
  const [openId, setOpenId] = useState("");
  const known = sessionRegistry(pid);
  return (
    <>
      <div className="row" style={{ justifyContent: "space-between" }}>
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
        <button className="btn primary" onClick={() => setCreating(true)}>
          + {app.t("New session", "新建会话")}
        </button>
      </div>
      <div className="banner gate">
        <span>◌</span>
        <span>
          {app.t(
            "Session listing needs GET /v1/sessions (roadmap §7.1) — below are sessions known to this browser.",
            "会话枚举需要 GET /v1/sessions(路线 §7.1)——下方为本浏览器已知的会话。",
          )}
        </span>
      </div>
      <div className="card" style={{ padding: 0 }}>
        <table className="table">
          <thead>
            <tr>
              <th>Session</th>
              <th>{app.t("Title", "标题")}</th>
              <th />
            </tr>
          </thead>
          <tbody>
            {known.map((s) => (
              <tr key={s.id} data-click="true" onClick={() => nav(`/p/${pid}/sessions/${s.id}`)}>
                <td className="mono">{s.id}</td>
                <td>{s.title || <span className="mut">(untitled)</span>}</td>
                <td style={{ textAlign: "right", color: "var(--fg3)" }}>▸</td>
              </tr>
            ))}
            {known.length === 0 && (
              <tr>
                <td colSpan={3} className="mut">
                  {app.t("No sessions yet.", "还没有会话。")}
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
