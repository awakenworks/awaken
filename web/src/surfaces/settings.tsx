// Workspace · Settings: the unified two-band rail. Sections link out to the
// full surfaces; identity + project authoring live here.

import { useMutation, useQueryClient } from "@tanstack/react-query";
import { useState } from "react";
import { useNavigate } from "react-router";
import { api } from "../lib/api/client";
import type { Project } from "../lib/api/types";
import { useApp } from "../lib/app-state";

const WORKSPACE = "wrkspc_default";

export default function SettingsSurface() {
  const app = useApp();
  const nav = useNavigate();
  const qc = useQueryClient();
  const [pid, setPid] = useState("");
  const [pname, setPname] = useState("");
  const createProject = useMutation({
    mutationFn: () =>
      api.put<Project>(`/v1/config/projects/${pid}`, {
        id: pid,
        workspace_id: WORKSPACE,
        display_name: pname || pid,
        version: 1,
      }),
    onSuccess: (p) => {
      setPid("");
      setPname("");
      app.setProjectId(p.id);
      void qc.invalidateQueries({ queryKey: ["projects"] });
    },
  });
  const link = (label: string, to: string, hint: string) => (
    <button className="nav-item" style={{ height: "auto", padding: "8px 10px" }} onClick={() => nav(to)}>
      <span style={{ display: "flex", flexDirection: "column", textAlign: "left" }}>
        <strong style={{ fontSize: 13 }}>{label}</strong>
        <span className="mut" style={{ fontSize: 11.5, whiteSpace: "normal" }}>
          {hint}
        </span>
      </span>
      <span style={{ marginLeft: "auto", color: "var(--fg3)" }}>↗</span>
    </button>
  );
  return (
    <>
      <div className="banner info">
        <span>⚑</span>
        <span>
          {app.t("Scoped to workspace", "作用域:工作区")} · <code>{WORKSPACE}</code> —{" "}
          {app.t("shared across every project.", "全部项目共享。")}
        </span>
      </div>
      <div style={{ display: "grid", gridTemplateColumns: "1fr 1fr", gap: 14 }}>
        <div className="card">
          <h2>Workspace</h2>
          {link("AI providers & models", "/models", app.t("Catalog, offerings, inference profiles, resolve dry-run", "目录、offering、profile 与 resolve 试算"))}
          {link("Credentials", "/credentials", app.t("Supply-side sources & pools — secret-in, secret-free-out", "供给侧凭证与池——只进不出"))}
          {link("MCP servers", "/mcp-servers", app.t("Authored definitions with fail-closed bindings", "作者化定义,fail-closed 绑定"))}
          {link("A2A servers", "/a2a-servers", app.t("Remote delegate directory", "远程委托目录"))}
          {link("Access", "/access", app.t("IAM tokens & roles", "IAM 令牌与角色"))}
        </div>
        <div className="card">
          <h2>{app.t("Projects", "项目")}</h2>
          <p className="hint">
            {app.t(
              "A project SELECTS from workspace supply and addresses the run plane at /projects/{id}.",
              "项目从工作区供给中选择,并以 /projects/{id} 寻址运行面。",
            )}
          </p>
          {app.projects.map((p) => (
            <div key={p.id} className="row" style={{ padding: "4px 0" }}>
              <code>{p.id}</code>
              <span>{p.display_name}</span>
              <button className="btn ghost" style={{ marginLeft: "auto", height: 24 }} onClick={() => nav(`/p/${p.id}/settings`)}>
                {app.t("open", "打开")}
              </button>
            </div>
          ))}
          <div className="row" style={{ marginTop: 10 }}>
            <input className="input mono" style={{ width: 140 }} placeholder="id (dns-safe)" value={pid} onChange={(e) => setPid(e.target.value)} />
            <input className="input" style={{ flex: 1 }} placeholder={app.t("display name", "显示名")} value={pname} onChange={(e) => setPname(e.target.value)} />
            <button className="btn primary" disabled={!pid || createProject.isPending} onClick={() => createProject.mutate()}>
              + {app.t("Create", "创建")}
            </button>
          </div>
          {createProject.error instanceof Error && <div className="err">{createProject.error.message}</div>}
        </div>
      </div>
    </>
  );
}
