import { ChevronDown, Search } from "@awaken/ui/icons";
import { AppearanceSelect, BrandMark } from "@awaken/ui/brand/react";
import { Button as SharedButton, MenuPopover, SuiteSwitcher } from "@awaken/ui";

import { useApp } from "../../lib/app-state";
import { suiteCloudUrl, suiteHubUrl, suiteProductEntryUrl, useSuiteNavigation } from "../../lib/suite-navigation";

export function openPalette() {
  window.dispatchEvent(new CustomEvent("awaken:open-palette"));
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
            { id: "awaken", label: "Awaken Agents", description: app.workspaceName, icon: <BrandMark mark="agents" />, isCurrent: true },
            { id: "flow", label: "Awaken Workforce", description: app.t("Plan work and coordinate outcomes", "规划工作并协同成果"), href: suiteProductEntryUrl(hubUrl, "flow"), icon: <BrandMark mark="workforce" /> },
          ]}
          trigger={<button className="brand-anchor brand-anchor--switcher" type="button">
            <BrandMark mark="agents" />
            <span className="brand-copy">
              <strong>Awaken</strong>
              <small>Agents</small>
            </span>
            <ChevronDown className="brand-chevron" />
          </button>}
        />
      ) : (
        <div className="brand-anchor" aria-label="Awaken Agents">
          <BrandMark mark="agents" />
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
          <Search />
          <span>{app.t("Search…", "搜索…")}</span>
          <kbd>⌘K</kbd>
        </button>
        <SharedButton className="chrome-btn" onClick={app.toggleLocale} title={app.t("Switch language", "切换语言")} aria-label={app.t("Switch language", "切换语言")}>
          {app.locale === "en" ? "EN" : "中"}
        </SharedButton>
        <AppearanceSelect labels={{
          label: app.t("Appearance", "外观"),
          system: app.t("System", "跟随系统"),
          light: app.t("Light", "浅色"),
          dark: app.t("Dark", "深色"),
        }} />
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
          <SharedButton className="chrome-btn identity-trigger" type="button" title={app.userName} aria-label={app.t(`Account: ${app.userName}`, `账号：${app.userName}`)}>
            {app.userName.trim().charAt(0).toUpperCase() || "U"}
          </SharedButton>
        </MenuPopover>
      </div>
    </header>
  );
}
