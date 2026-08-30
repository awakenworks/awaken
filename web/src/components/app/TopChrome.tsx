import { MenuPopover } from "@awaken/ui";
import { useApp } from "../../lib/app-state";
import { suiteHubUrl, useSuiteNavigation } from "../../lib/suite-navigation";

export function openPalette() {
  window.dispatchEvent(new CustomEvent("awaken:open-palette"));
}

function AwakenMark() {
  return (
    <svg className="awaken-mark" aria-hidden="true" viewBox="0 0 32 32" fill="none">
      <path data-silhouette="A" d="M14.2 6h3.6l8.6 20h-3L16 8.4 8.6 26h-3Z" fill="var(--mark-primary)" />
      <circle data-role="decision" cx="16" cy="20.2" r="2.5" fill="var(--mark-decision)" />
    </svg>
  );
}

function SearchIcon() {
  return <svg aria-hidden="true" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2"><circle cx="11" cy="11" r="7" /><path d="m20 20-3.4-3.4" /></svg>;
}

function ThemeIcon({ dark }: { dark: boolean }) {
  return dark
    ? <svg aria-hidden="true" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2"><path d="M20 15.4A8 8 0 0 1 8.6 4a8 8 0 1 0 11.4 11.4Z" /></svg>
    : <svg aria-hidden="true" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2"><circle cx="12" cy="12" r="3.5" /><path d="M12 2v2M12 20v2M4.9 4.9l1.4 1.4M17.7 17.7l1.4 1.4M2 12h2M20 12h2M4.9 19.1l1.4-1.4M17.7 6.3l1.4-1.4" /></svg>;
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
                <span className="suite-grid-icon" aria-hidden="true"><svg viewBox="0 0 24 24" fill="currentColor"><rect x="4" y="4" width="6" height="6" rx="1" /><rect x="14" y="4" width="6" height="6" rx="1" /><rect x="4" y="14" width="6" height="6" rx="1" /><rect x="14" y="14" width="6" height="6" rx="1" /></svg></span>
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
      <div className="workspace-context" title={app.workspaceId}>
        <span className="mut">{app.t("Workspace", "工作区")}</span>
        <strong>{app.workspaceName}</strong>
      </div>

      <span style={{ flex: 1 }} />
      <div className="topbar-actions">
        <button className="search-box" onClick={openPalette}>
          <SearchIcon />
          <span>{app.t("Search…", "搜索…")}</span>
          <kbd>⌘K</kbd>
        </button>
        <button className="chrome-btn" onClick={app.toggleLocale} title={app.t("Switch language", "切换语言")} aria-label={app.t("Switch language", "切换语言")}>
          {app.locale === "en" ? "EN" : "中"}
        </button>
        <button className="chrome-btn" onClick={app.toggleTheme} title={app.t("Switch theme", "切换主题")} aria-label={app.t("Switch theme", "切换主题")}>
          <ThemeIcon dark={app.theme === "dark"} />
        </button>
        <MenuPopover
          aria-label={app.t("Account and organization", "账号与组织")}
          content={<div className="identity-menu">
            <small>{app.t("Signed in as", "当前用户")}</small>
            <strong>{app.userName}</strong>
            <span>{app.organizationName}</span>
            <span className="mono mut" title={app.workspaceId}>{app.workspaceName}</span>
          </div>}
          placement="bottom-end"
        >
          <button className="chrome-btn identity-trigger" type="button" title={app.userName} aria-label={app.t(`Account: ${app.userName}`, `账号：${app.userName}`)}>
            {app.userName.trim().charAt(0).toUpperCase() || "U"}
          </button>
        </MenuPopover>
      </div>
    </header>
  );
}
