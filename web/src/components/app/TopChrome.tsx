// The topbar chrome (design handoff): the org anchor + workspace-switcher
// breadcrumb live here (not the rail), then a spacer, the ⌘K search box, and the
// locale / theme / token controls. The workspace switcher is the tenancy scope
// picker (client-held roster).

import { useState } from "react";
import { useNavigate } from "react-router";
import { getToken, setToken } from "../../lib/api/client";
import { useApp } from "../../lib/app-state";

/** Ask the command palette (in AppShell) to open. */
export function openPalette() {
  window.dispatchEvent(new CustomEvent("awaken:open-palette"));
}

function WorkspaceSwitcher() {
  const app = useApp();
  const nav = useNavigate();
  const [open, setOpen] = useState(false);
  const [newWs, setNewWs] = useState("");
  const active = app.workspaces.find((w) => w.id === app.workspaceId);

  return (
    <div style={{ position: "relative", flex: "none" }}>
      <button className="ws-crumb" onClick={() => setOpen((v) => !v)}>
        <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2">
          <path d="M4 20h16a2 2 0 0 0 2-2V8a2 2 0 0 0-2-2h-7.9a2 2 0 0 1-1.69-.9L9.6 3.9A2 2 0 0 0 7.93 3H4a2 2 0 0 0-2 2v13c0 1.1.9 2 2 2Z" />
        </svg>
        <span>{active?.display_name || app.workspaceId}</span>
        <span className="mut" style={{ color: "inherit" }}>▾</span>
      </button>
      {open && (
        <>
          <div style={{ position: "fixed", inset: 0, zIndex: 39 }} onClick={() => setOpen(false)} />
          <div
            style={{
              position: "absolute",
              top: "100%",
              left: 0,
              marginTop: 6,
              minWidth: 240,
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
                  setOpen(false);
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
                    setOpen(false);
                    nav(`/w/${id}/overview`);
                  }
                }}
              />
            </div>
          </div>
        </>
      )}
    </div>
  );
}

function TokenButton() {
  const app = useApp();
  const [open, setOpen] = useState(false);
  const [value, setValue] = useState(getToken());
  return (
    <>
      <button className="chrome-btn" onClick={() => setOpen(true)} title={app.t("IAM bearer token", "IAM 令牌")}>
        <span className="dot" style={{ background: getToken() ? "var(--ok)" : "var(--fg3)" }} />
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
            <input className="input mono" value={value} placeholder="sk-ant-…" onChange={(e) => setValue(e.target.value)} />
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

export default function TopChrome() {
  const app = useApp();
  return (
    <header className="topbar">
      <button className="org-anchor">
        <span className="org-badge">A</span>
        <span style={{ display: "flex", flexDirection: "column", textAlign: "left", lineHeight: 1.1 }}>
          <span style={{ fontSize: 14, fontWeight: 600, color: "var(--fg)" }}>Awaken</span>
          <span style={{ fontSize: 9.5, fontWeight: 600, letterSpacing: ".06em", textTransform: "uppercase", color: "var(--fg3)" }}>
            {app.t("Organization", "组织")}
          </span>
        </span>
        <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="var(--fg3)" strokeWidth="2">
          <path d="m6 9 6 6 6-6" />
        </svg>
      </button>
      <span className="crumb-sep" />
      <WorkspaceSwitcher />

      <span style={{ flex: 1 }} />

      <div className="row" style={{ gap: 8 }}>
        <button className="search-box" onClick={openPalette}>
          <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2">
            <circle cx="11" cy="11" r="8" />
            <path d="m21 21-4.3-4.3" />
          </svg>
          <span>{app.t("Search…", "搜索…")}</span>
          <kbd>⌘K</kbd>
        </button>
        <button className="chrome-btn" onClick={app.toggleLocale} title="Language">
          {app.locale === "en" ? "EN" : "中"}
        </button>
        <button className="chrome-btn" onClick={app.toggleTheme} title="Theme">
          {app.theme === "dark" ? "☾" : "☀"}
        </button>
        <TokenButton />
      </div>
    </header>
  );
}
