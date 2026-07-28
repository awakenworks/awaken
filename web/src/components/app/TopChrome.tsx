import { useApp } from "../../lib/app-state";

export function openPalette() {
  window.dispatchEvent(new CustomEvent("awaken:open-palette"));
}

function AwakenMark() {
  return (
    <span className="awaken-mark" aria-hidden="true">
      <span>A</span>
      <i />
    </span>
  );
}

export default function TopChrome() {
  const app = useApp();
  return (
    <header className="topbar">
      <div className="brand-anchor" aria-label="Awaken Agents">
        <AwakenMark />
        <span className="brand-copy">
          <strong>Awaken</strong>
          <small>Agents</small>
        </span>
      </div>
      <span className="crumb-sep" />
      <div className="workspace-context">
        <span className="mut">{app.t("Workspace", "工作区")}</span>
        <strong>{app.workspaceId}</strong>
      </div>

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
      </div>
    </header>
  );
}
