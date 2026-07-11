// Workspace · Settings: the supply/govern rail for the active workspace. Sections
// link out to the full surfaces. Tenancy is Org ▸ Workspace (ADR-0051): a workspace
// owns both its run resources and its config; Project was removed. Workspaces are a
// client-held roster (no server registry) — managed from the sidebar switcher.

import { useNavigate, useParams } from "react-router";
import { getWorkspace } from "../lib/api/client";
import { useApp } from "../lib/app-state";
import { Button, Card, Pill } from "../components/ui";

export default function SettingsSurface() {
  const app = useApp();
  const nav = useNavigate();
  const { ws: wsId = "default" } = useParams();

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

  const base = `/w/${wsId}`;
  return (
    <>
      <div className="banner info">
        <span>⚑</span>
        <span>
          {app.t("Scoped to workspace", "作用域:工作区")} · <code>{wsId}</code> —{" "}
          {app.t(
            getWorkspace() ? "addressed at /v1/workspaces/{ws}/…" : "default scope (flat /v1/…)",
            getWorkspace() ? "以 /v1/workspaces/{ws}/… 寻址" : "默认作用域(扁平 /v1/…)",
          )}
        </span>
      </div>
      <div style={{ display: "grid", gridTemplateColumns: "1fr 1fr", gap: 14 }}>
        <Card>
          <h2>{app.t("Supply & governance", "供给与治理")}</h2>
          {link("AI providers & models", `${base}/models`, app.t("Catalog, offerings, inference profiles, resolve dry-run", "目录、offering、profile 与 resolve 试算"))}
          {link("Credentials", `${base}/credentials`, app.t("Supply-side sources & pools — secret-in, secret-free-out", "供给侧凭证与池——只进不出"))}
          {link("MCP servers", `${base}/mcp-servers`, app.t("Authored definitions with fail-closed bindings", "作者化定义,fail-closed 绑定"))}
          {link("A2A servers", `${base}/a2a-servers`, app.t("Remote delegate directory", "远程委托目录"))}
          {link("Access", `${base}/access`, app.t("IAM tokens & roles", "IAM 令牌与角色"))}
        </Card>
        <Card>
          <h2>{app.t("Workspaces", "工作区")}</h2>
          <p className="hint">
            {app.t(
              "Org ▸ Workspace: a workspace is the tenancy scope, owning both run resources and config. The roster is client-held (no server registry) — add/switch from the sidebar. \"default\" is the flat default scope.",
              "Org ▸ Workspace:workspace 是租户作用域,同时拥有运行资源与配置。名册由客户端持有(无服务端注册)——在侧栏添加/切换。\"default\" 即扁平默认作用域。",
            )}
          </p>
          {app.workspaces.map((w) => (
            <div key={w.id} className="row" style={{ padding: "4px 0" }}>
              <code>{w.id}</code>
              <span>{w.display_name}</span>
              {w.id === app.workspaceId && <Pill tone="ok">{app.t("active", "当前")}</Pill>}
              <Button
                variant="ghost"
                style={{ marginLeft: "auto", height: 24 }}
                onClick={() => {
                  app.setWorkspaceId(w.id);
                  nav(`/w/${w.id}/overview`);
                }}
              >
                {app.t("enter", "进入")}
              </Button>
            </div>
          ))}
        </Card>
      </div>
    </>
  );
}
