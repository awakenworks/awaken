import { MenuPopover } from "@awaken/ui";
import { useApp } from "../../lib/app-state";
import { suiteHubUrl, useSuiteNavigation } from "../../lib/suite-navigation";

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
  const suiteNavigation = useSuiteNavigation();
  const hubUrl = suiteHubUrl(suiteNavigation.data, suiteNavigation.isError);
  return (
    <header className="topbar">
      {hubUrl ? (
        <MenuPopover
          aria-label={app.t("Awaken products", "Awaken 产品")}
          content={(
            <div className="suite-menu">
              <div className="suite-menu__current">
                <span className="suite-menu__mark"><AwakenMark /></span>
                <span><small>{app.t("Current product", "当前产品")}</small><strong>Awaken Agents</strong></span>
              </div>
              <a href={hubUrl} role="menuitem">
                <span className="suite-grid-icon" aria-hidden="true">••<br />••</span>
                <span><strong>{app.t("All products", "所有产品")}</strong><small>{app.t("Switch products, manage usage and billing", "切换产品、管理用量与账单")}</small></span>
                <span aria-hidden="true">→</span>
              </a>
            </div>
          )}
          placement="bottom-start"
        >
          <button className="brand-anchor brand-anchor--switcher" type="button">
            <AwakenMark />
            <span className="brand-copy">
              <strong>Awaken</strong>
              <small>Agents</small>
            </span>
            <span className="brand-chevron" aria-hidden="true">⌄</span>
          </button>
        </MenuPopover>
      ) : (
        <div className="brand-anchor" aria-label="Awaken Agents">
          <AwakenMark />
          <span className="brand-copy">
            <strong>Awaken</strong>
            <small>Agents</small>
          </span>
        </div>
      )}
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
