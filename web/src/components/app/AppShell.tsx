import { useEffect, useState } from "react";
import { Outlet, useLocation, useNavigate } from "react-router";
import { getToken, setToken } from "../../lib/api/client";
import { ConfirmProvider } from "../ui/Confirm";
import { ToastProvider } from "../ui/Toast";
import { useApp } from "../../lib/app-state";
import { NAV, navPath, titleForPath } from "../../lib/navigation/paths";
import type { NavGroup } from "../../lib/navigation/paths";

const GROUP_CAPTIONS: Partial<Record<NavGroup, [string, string]>> = {
  run: ["Run", "运行"],
  supply: ["Supply", "供给"],
  observe: ["Observe", "观测"],
  govern: ["Govern", "治理"],
};

function Sidebar() {
  const app = useApp();
  const nav = useNavigate();
  const location = useLocation();
  const [wsMenu, setWsMenu] = useState(false);
  const [newWs, setNewWs] = useState("");

  const groups: NavGroup[] = ["run", "supply", "observe", "govern"];

  const isActive = (path: string) => {
    const concrete = path.replace(":ws", app.workspaceId || "default");
    return concrete === "/" ? location.pathname === "/" : location.pathname.startsWith(concrete);
  };
  const activeWs = app.workspaces.find((w) => w.id === app.workspaceId);

  return (
    <aside className="sidebar">
      {/* Org level — single implicit org (no backend Org resource yet). */}
      <div className="ws-row" style={{ cursor: "default" }}>
        <span
          style={{
            width: 26,
            height: 26,
            borderRadius: 7,
            background: "var(--fg)",
            color: "var(--canvas)",
            display: "inline-flex",
            alignItems: "center",
            justifyContent: "center",
            fontWeight: 700,
            fontSize: 13,
          }}
        >
          A
        </span>
        <span style={{ display: "flex", flexDirection: "column", flex: 1, minWidth: 0 }}>
          <strong style={{ fontSize: 13 }}>Awaken</strong>
          <span className="mut" style={{ fontSize: 10.5 }}>
            {app.t("Organization", "组织")}
          </span>
        </span>
      </div>

      {/* Workspace switcher — the tenancy scope. Client-held roster (no server list). */}
      <div style={{ position: "relative" }}>
        <button className="proj-row" data-active onClick={() => setWsMenu((v) => !v)}>
          <span>🗂️</span>
          <span style={{ flex: 1, minWidth: 0, fontWeight: 600, overflow: "hidden", textOverflow: "ellipsis" }}>
            {activeWs?.display_name || app.workspaceId}
          </span>
          <span className="mut" style={{ fontSize: 10, textTransform: "uppercase", letterSpacing: ".04em" }}>
            Workspace
          </span>
          <span className="mut">▾</span>
        </button>
        {wsMenu && (
          <>
            <div style={{ position: "fixed", inset: 0, zIndex: 39 }} onClick={() => setWsMenu(false)} />
            <div
              style={{
                position: "absolute",
                top: "100%",
                left: 8,
                right: 0,
                zIndex: 40,
                background: "var(--surface)",
                borderRadius: 11,
                boxShadow: "var(--shadow-pop)",
                padding: 5,
              }}
            >
              <div className="nav-caption">{app.t("Switch workspace", "切换工作区")}</div>
              {app.workspaces.map((w) => (
                <button
                  key={w.id}
                  className="nav-item"
                  data-active={w.id === app.workspaceId}
                  onClick={() => {
                    app.setWorkspaceId(w.id);
                    setWsMenu(false);
                    nav(`/w/${w.id}/overview`);
                  }}
                >
                  {w.display_name || w.id}
                  <span className="mono mut" style={{ marginLeft: "auto" }}>
                    {w.id}
                  </span>
                </button>
              ))}
              <div className="nav-caption">{app.t("Add workspace (scope id)", "添加工作区(作用域 id)")}</div>
              <div className="row" style={{ padding: "2px 6px 6px" }}>
                <input
                  className="input mono"
                  style={{ flex: 1, height: 26 }}
                  placeholder="ws_acme"
                  value={newWs}
                  onChange={(e) => setNewWs(e.target.value)}
                  onKeyDown={(e) => {
                    if (e.key === "Enter" && newWs.trim()) {
                      const id = newWs.trim();
                      app.addWorkspace(id);
                      setNewWs("");
                      setWsMenu(false);
                      nav(`/w/${id}/overview`);
                    }
                  }}
                />
              </div>
            </div>
          </>
        )}
      </div>

      <div className="nav-caption">{app.t("Global", "全局")}</div>
      {NAV.filter((n) => n.group === "global").map((n) => (
        <button key={n.key} className="nav-item" data-active={isActive(n.path)} onClick={() => nav(navPath(n, app.workspaceId))}>
          {app.t(n.label, n.labelZh)}
        </button>
      ))}
      <div style={{ height: 1, background: "var(--line)", margin: "9px 8px 3px" }} />

      {groups.map((g) => (
        <div key={g}>
          <div className="nav-caption">{app.t(GROUP_CAPTIONS[g]?.[0] ?? g, GROUP_CAPTIONS[g]?.[1] ?? g)}</div>
          {NAV.filter((n) => n.group === g).map((n) => (
            <button
              key={n.key}
              className="nav-item"
              data-active={isActive(n.path)}
              onClick={() => nav(navPath(n, app.workspaceId))}
              title={n.gated ? app.t("Backend face not mounted yet", "后端面尚未就绪") : undefined}
            >
              {app.t(n.label, n.labelZh)}
              {n.gated && <span className="mut" style={{ marginLeft: "auto", fontSize: 10 }}>◌</span>}
            </button>
          ))}
        </div>
      ))}

      <div className="sidebar-footer">
        <button className="btn ghost" style={{ height: 26, padding: "0 9px" }} onClick={app.toggleLocale}>
          {app.locale === "en" ? "EN" : "中"}
        </button>
        <button className="btn ghost" style={{ height: 26, padding: "0 9px" }} onClick={app.toggleTheme}>
          {app.theme === "dark" ? "☾" : "☀"}
        </button>
        <span style={{ flex: 1 }} />
        <TokenButton />
      </div>
    </aside>
  );
}

function TokenButton() {
  const app = useApp();
  const [open, setOpen] = useState(false);
  const [value, setValue] = useState(getToken());
  return (
    <>
      <button
        className="btn ghost"
        style={{ height: 26, padding: "0 9px" }}
        onClick={() => setOpen(true)}
        title={app.t("IAM bearer token", "IAM 令牌")}
      >
        <span className="dot" style={{ background: getToken() ? "var(--ok)" : "var(--fg3)" }} />
        key
      </button>
      {open && (
        <div className="overlay" onClick={() => setOpen(false)}>
          <div className="modal" onClick={(e) => e.stopPropagation()}>
            <h3>{app.t("API token", "API 令牌")}</h3>
            <p className="mut" style={{ margin: 0 }}>
              {app.t(
                "Bearer token for the management plane (embedded IAM). Sessions run unguarded in P1.",
                "管理面(嵌入式 IAM)的 Bearer 令牌;P1 阶段会话面不设门。",
              )}
            </p>
            <input
              className="input mono"
              value={value}
              placeholder="sk-ant-…"
              onChange={(e) => setValue(e.target.value)}
            />
            <div className="row" style={{ justifyContent: "flex-end" }}>
              <button
                className="btn"
                onClick={() => {
                  setToken("");
                  setValue("");
                }}
              >
                {app.t("Clear", "清除")}
              </button>
              <button
                className="btn primary"
                onClick={() => {
                  setToken(value.trim());
                  setOpen(false);
                }}
              >
                {app.t("Save", "保存")}
              </button>
            </div>
          </div>
        </div>
      )}
    </>
  );
}

function CommandPalette() {
  const app = useApp();
  const nav = useNavigate();
  const [open, setOpen] = useState(false);
  const [q, setQ] = useState("");
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if ((e.metaKey || e.ctrlKey) && e.key.toLowerCase() === "k") {
        e.preventDefault();
        setOpen((v) => !v);
        setQ("");
      }
      if (e.key === "Escape") setOpen(false);
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, []);
  if (!open) return null;
  const hits = NAV.filter(
    (n) => n.label.toLowerCase().includes(q.toLowerCase()) || n.labelZh.includes(q),
  ).slice(0, 8);
  return (
    <div className="overlay" onClick={() => setOpen(false)}>
      <div className="modal" onClick={(e) => e.stopPropagation()}>
        <input
          autoFocus
          className="input"
          placeholder={app.t("Go to…", "跳转…")}
          value={q}
          onChange={(e) => setQ(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === "Enter" && hits[0]) {
              nav(navPath(hits[0], app.workspaceId));
              setOpen(false);
            }
          }}
        />
        {hits.map((n) => (
          <button
            key={n.key}
            className="nav-item"
            onClick={() => {
              nav(navPath(n, app.workspaceId));
              setOpen(false);
            }}
          >
            {app.t(n.label, n.labelZh)}
            <span className="mut" style={{ marginLeft: "auto", fontSize: 11 }}>
              {n.group}
            </span>
          </button>
        ))}
      </div>
    </div>
  );
}

export default function AppShell() {
  const location = useLocation();
  const crumbs = titleForPath(location.pathname);
  return (
    <ToastProvider>
      <ConfirmProvider>
        <div className="shell">
          <div className="dawn" />
          <Sidebar />
          <main className="main">
            <header className="topbar">
              <span>Awaken</span>
              {crumbs.scope && <span style={{ color: "var(--line-strong)" }}>/</span>}
              <span>{crumbs.scope}</span>
              {crumbs.title && <span style={{ color: "var(--line-strong)" }}>/</span>}
              <span className="crumb-title">{crumbs.title}</span>
              <span style={{ flex: 1 }} />
              <kbd className="mono mut" style={{ fontSize: 10.5 }}>
                ⌘K
              </kbd>
            </header>
            <div className="content">
              <div className="content-inner">
                <Outlet />
              </div>
            </div>
          </main>
          <CommandPalette />
        </div>
      </ConfirmProvider>
    </ToastProvider>
  );
}
