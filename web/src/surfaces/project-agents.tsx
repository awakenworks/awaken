// Project · Agents: the Managed Agents registry (/v1/agents) — persisted,
// versioned agent configs. Create once, reference by id from sessions. MCP is
// declared INLINE ({type,name,url}); runtime credentials come from the project
// Vault. The model references the workspace catalog by id (reference-don't-copy).

import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useState } from "react";
import Drawer from "../components/ui/Drawer";
import { api } from "../lib/api/client";
import type { Agent, Page, ProviderCatalog } from "../lib/api/types";
import { useApp } from "../lib/app-state";
import ModelsSurface from "./models";

function modelId(m: Agent["model"]): string {
  return typeof m === "string" ? m : m.id;
}

function CreateModal({ onClose }: { onClose: () => void }) {
  const app = useApp();
  const qc = useQueryClient();
  const [name, setName] = useState("");
  const [model, setModel] = useState("");
  const [system, setSystem] = useState("You are a helpful coding agent.");
  const [mcp, setMcp] = useState<{ name: string; url: string }[]>([]);
  const [manageModels, setManageModels] = useState(false);
  const catalog = useQuery({
    queryKey: ["catalog"],
    queryFn: () => api.get<ProviderCatalog>("/v1/config/catalog"),
  });
  const models = Array.from(new Set((catalog.data?.offerings ?? []).map((o) => o.model_id)));
  const create = useMutation({
    mutationFn: () =>
      api.post<Agent>("/v1/agents", {
        name: name || "agent",
        model,
        system: system || undefined,
        mcp_servers: mcp
          .filter((m) => m.name && m.url)
          .map((m) => ({ type: "url", name: m.name, url: m.url })),
      }),
    onSuccess: () => {
      void qc.invalidateQueries({ queryKey: ["agents"] });
      onClose();
    },
  });
  return (
    <div className="overlay" onClick={onClose}>
      <div className="modal" onClick={(e) => e.stopPropagation()}>
        <h3>{app.t("New agent", "新建 Agent")}</h3>
        <div className="field">
          <label>{app.t("Name", "名称")}</label>
          <input className="input" value={name} onChange={(e) => setName(e.target.value)} placeholder="Coding Assistant" />
        </div>
        <div className="field">
          <label className="row" style={{ justifyContent: "space-between" }}>
            <span>{app.t("Model (references workspace catalog)", "模型(引用工作区 catalog)")}</span>
            <button className="manage-link" onClick={() => setManageModels(true)}>
              {app.t("Manage ↗", "管理 ↗")}
            </button>
          </label>
          {models.length > 0 ? (
            <select className="input mono" value={model} onChange={(e) => setModel(e.target.value)}>
              <option value="">{app.t("— select a model —", "— 选择模型 —")}</option>
              {models.map((m) => (
                <option key={m} value={m}>
                  {m}
                </option>
              ))}
            </select>
          ) : (
            <input
              className="input mono"
              value={model}
              onChange={(e) => setModel(e.target.value)}
              placeholder="kimi-k2"
            />
          )}
        </div>
        <div className="field">
          <label>{app.t("System prompt", "系统提示词")}</label>
          <textarea className="input mono" rows={4} value={system} onChange={(e) => setSystem(e.target.value)} />
        </div>
        <div className="field">
          <label>{app.t("Inline MCP servers (credentials from Vault by url)", "内联 MCP(凭证由 Vault 按 url 提供)")}</label>
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
            + {app.t("add server", "添加服务器")}
          </button>
        </div>
        {create.error instanceof Error && <div className="err">{create.error.message}</div>}
        <div className="row" style={{ justifyContent: "flex-end" }}>
          <button className="btn" onClick={onClose}>
            {app.t("Cancel", "取消")}
          </button>
          <button className="btn primary" disabled={create.isPending} onClick={() => create.mutate()}>
            {app.t("Create", "创建")}
          </button>
        </div>
      </div>
      {manageModels && (
        <Drawer title={app.t("Models", "模型")} onClose={() => setManageModels(false)}>
          <ModelsSurface />
        </Drawer>
      )}
    </div>
  );
}

export default function ProjectAgentsSurface() {
  const app = useApp();
  const qc = useQueryClient();
  const [creating, setCreating] = useState(false);
  const agents = useQuery({
    queryKey: ["agents"],
    queryFn: () => api.get<Page<Agent>>("/v1/agents"),
    refetchInterval: 30_000,
  });
  const archive = useMutation({
    mutationFn: (id: string) => api.post(`/v1/agents/${id}/archive`),
    onSuccess: () => void qc.invalidateQueries({ queryKey: ["agents"] }),
  });
  const rows = agents.data?.data ?? [];

  return (
    <>
      <p className="mut" style={{ margin: 0 }}>
        {app.t(
          "Persisted, versioned agent configs. Create once, reference by id from sessions; each update mints a new version. Archive is permanent.",
          "持久化、带版本的 agent 配置。创建一次、session 按 id 引用;每次更新产生新版本。归档不可逆。",
        )}
      </p>
      <div className="row" style={{ justifyContent: "space-between" }}>
        <span className="mut">
          {app.t("MCP is inline ({type,name,url}); credentials come from the project Vault.", "MCP 内联({type,name,url});凭证来自 Project Vault。")}
        </span>
        <button className="btn primary" onClick={() => setCreating(true)}>
          + {app.t("New agent", "新建 Agent")}
        </button>
      </div>
      {agents.error instanceof Error && <div className="err">{agents.error.message}</div>}
      <div className="card" style={{ padding: 0 }}>
        <table className="table">
          <thead>
            <tr>
              <th>Agent</th>
              <th>{app.t("Name", "名称")}</th>
              <th>{app.t("Model", "模型")}</th>
              <th>MCP</th>
              <th>v</th>
              <th />
            </tr>
          </thead>
          <tbody>
            {rows.map((a) => (
              <tr key={a.id}>
                <td className="mono">{a.id}</td>
                <td>
                  <span className="pill agent">{a.name}</span>
                </td>
                <td className="mono mut">{modelId(a.model)}</td>
                <td className="mut">{a.mcp_servers?.length ?? 0}</td>
                <td className="mono mut">{a.version}</td>
                <td style={{ textAlign: "right" }}>
                  {a.archived_at ? (
                    <span className="pill neutral">{app.t("archived", "已归档")}</span>
                  ) : (
                    <button
                      className="btn ghost"
                      style={{ height: 22 }}
                      disabled={archive.isPending}
                      onClick={() => archive.mutate(a.id)}
                    >
                      {app.t("Archive", "归档")}
                    </button>
                  )}
                </td>
              </tr>
            ))}
            {rows.length === 0 && (
              <tr>
                <td colSpan={6} className="mut">
                  {agents.isLoading ? "…" : app.t("No agents yet.", "还没有 Agent。")}
                </td>
              </tr>
            )}
          </tbody>
        </table>
      </div>
      {creating && <CreateModal onClose={() => setCreating(false)} />}
    </>
  );
}
