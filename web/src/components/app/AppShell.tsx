import { useEffect, useState } from "react";
import { Outlet, useLocation, useNavigate } from "react-router";
import { getToken, setToken } from "../../lib/api/client";
import { useApp } from "../../lib/app-state";
import { NAV, navPath, titleForPath } from "../../lib/navigation/paths";
import type { NavGroup } from "../../lib/navigation/paths";

const GROUP_CAPTIONS: Partial<Record<NavGroup, [string, string]>> = {
  supply: ["Workspace · Supply", "工作区 · 供给"],
  observe: ["Observe", "观测"],
  govern: ["Govern", "治理"],
};

function Sidebar() {
  const app = useApp();
  const nav = useNavigate();
  const location = useLocation();
  const [projMenu, setProjMenu] = useState(false);

  const groups: NavGroup[] =
    app.scope === "project" ? ["project"] : ["supply", "observe", "govern"];

  const isActive = (path: string) => {
    const concrete = path.replace(":pid", app.projectId || "-");
    return concrete === "/" ? location.pathname === "/" : location.pathname.startsWith(concrete);
  };

  return (
    <aside className="sidebar">
      <button
        className="ws-row"
        data-active={app.scope === "workspace"}
        onClick={() => app.setScope("workspace")}
      >
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
            Workspace
          </span>
        </span>
      </button>

      <div style={{ position: "relative" }}>
        <button
          className="proj-row"
          data-active={app.scope === "project"}
          onClick={() => setProjMenu((v) => !v)}
        >
          <span>📁</span>
          <span style={{ flex: 1, minWidth: 0, fontWeight: 600, overflow: "hidden", textOverflow: "ellipsis" }}>
            {app.projectId || app.t("no project", "无项目")}
          </span>
          <span className="mut" style={{ fontSize: 10, textTransform: "uppercase", letterSpacing: ".04em" }}>
            Project
          </span>
          <span className="mut">▾</span>
        </button>
        {projMenu && (
          <>
            <div style={{ position: "fixed", inset: 0, zIndex: 39 }} onClick={() => setProjMenu(false)} />
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
              <div className="nav-caption">{app.t("Switch project", "切换项目")}</div>
              {app.projects.map((p) => (
                <button
                  key={p.id}
                  className="nav-item"
                  data-active={p.id === app.projectId}
                  onClick={() => {
                    app.setProjectId(p.id);
                    app.setScope("project");
                    setProjMenu(false);
                    nav(`/p/${p.id}/overview`);
                  }}
                >
                  {p.display_name || p.id}
                  <span className="mono mut" style={{ marginLeft: "auto" }}>
                    {p.id}
                  </span>
                </button>
              ))}
              {app.projects.length === 0 && (
                <div className="mut" style={{ padding: "6px 10px" }}>
                  {app.t("No projects authored yet — create one in Settings.", "尚无项目——到项目设置里创建。")}
                </div>
              )}
              <button
                className="nav-item"
                onClick={() => {
                  app.setScope("workspace");
                  setProjMenu(false);
                  nav("/settings");
                }}
              >
                {app.t("All projects · Workspace", "全部项目 · 工作区")}
              </button>
            </div>
          </>
        )}
      </div>

      <div className="nav-caption">{app.t("Global · all projects", "全局 · 所有项目")}</div>
      {NAV.filter((n) => n.group === "global").map((n) => (
        <button key={n.key} className="nav-item" data-active={isActive(n.path)} onClick={() => nav(navPath(n, app.projectId))}>
          {app.t(n.label, n.labelZh)}
        </button>
      ))}
      <div style={{ height: 1, background: "var(--line)", margin: "9px 8px 3px" }} />

      {groups.map((g) => (
        <div key={g}>
          <div className="nav-caption">
            {g === "project"
              ? `Project · ${app.projectId || "—"}`
              : app.t(GROUP_CAPTIONS[g]?.[0] ?? g, GROUP_CAPTIONS[g]?.[1] ?? g)}
          </div>
          {NAV.filter((n) => n.group === g).map((n) => (
            <button
              key={n.key}
              className="nav-item"
              data-active={isActive(n.path)}
              onClick={() => nav(navPath(n, app.projectId))}
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
              nav(navPath(hits[0], app.projectId));
              setOpen(false);
            }
          }}
        />
        {hits.map((n) => (
          <button
            key={n.key}
            className="nav-item"
            onClick={() => {
              nav(navPath(n, app.projectId));
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
  );
}
