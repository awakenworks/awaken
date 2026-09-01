import { MenuPopover, SuiteSwitcher } from "@awaken/ui";
import { useApp } from "../../lib/app-state";
import { suiteCloudUrl, suiteHubUrl, suiteProductEntryUrl, useSuiteNavigation } from "../../lib/suite-navigation";

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
        <SuiteSwitcher
          aria-label={app.t("Awaken products", "Awaken 产品")}
          currentLabel={app.t("Current product and Workspace", "当前产品与工作区")}
          destinations={[
            { id: "products", label: app.t("All products and Workspaces", "所有产品与工作区"), href: suiteCloudUrl(hubUrl, "/products") },
            { id: "billing", label: app.t("Usage & Billing", "用量与账单"), href: suiteCloudUrl(hubUrl, "/usage-billing") },
            { id: "settings", label: app.t("Cloud settings", "Cloud 设置"), href: suiteCloudUrl(hubUrl, "/settings") },
          ]}
          placement="bottom-start"
          products={[
            { id: "awaken", label: "Awaken Agents", description: app.workspaceName, icon: <AwakenMark />, isCurrent: true },
            { id: "flow", label: "Awaken Flow", description: app.t("Plan work and coordinate outcomes", "规划工作并协同成果"), href: suiteProductEntryUrl(hubUrl, "flow"), icon: <svg viewBox="0 0 24 24" fill="currentColor"><path d="M5 4h6v6H5zM13 4h6v6h-6zM5 12h6v8H5zM13 12h6v8h-6z" /></svg> },
          ]}
          trigger={<button className="brand-anchor brand-anchor--switcher" type="button">
            <AwakenMark />
            <span className="brand-copy">
              <strong>Awaken</strong>
              <small>Agents</small>
            </span>
            <span className="brand-chevron" aria-hidden="true">⌄</span>
          </button>}
        />
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
            {hubUrl ? <a href={suiteCloudUrl(hubUrl, "/logout")} role="menuitem">{app.t("Sign out of Awaken Cloud", "退出 Awaken Cloud")}</a> : null}
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
