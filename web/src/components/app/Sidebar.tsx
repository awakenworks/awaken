import { useLocation, useNavigate } from "react-router";
import { navPath, visibleNavigation, type NavGroup, type NavItem } from "../../lib/navigation/paths";
import { useApp } from "../../lib/app-state";
import { useConfigCapabilities } from "../../lib/useConfigCapabilities";
import { pageIntentForPath } from "../../lib/page-intents";

export const GROUP_CAPTIONS: Record<NavGroup, [string, string]> = {
  workspace: ["Control plane", "控制面"],
  author: ["Build", "构建"],
  run: ["Run", "运行"],
  connect: ["Connect", "连接"],
  govern: ["Govern", "治理"],
};

function SidebarItem({ item }: { item: NavItem }) {
  const app = useApp();
  const nav = useNavigate();
  const location = useLocation();
  const concrete = navPath(item, app.workspaceId);
  return (
    <button
      className="nav-item"
      data-active={location.pathname.startsWith(concrete)}
      onClick={() => nav(concrete)}
    >
      {app.t(item.label, item.labelZh)}
    </button>
  );
}

export default function Sidebar() {
  const app = useApp();
  const nav = useNavigate();
  const location = useLocation();
  const capabilities = useConfigCapabilities();
  const navigation = visibleNavigation(capabilities.data);
  const groups: NavGroup[] = ["workspace", "author", "run", "connect", "govern"];
  const matchedPath = navigation
    .map((item) => navPath(item, app.workspaceId))
    .find((path) => location.pathname.startsWith(path));
  const activePath = matchedPath ?? location.pathname;
  const currentIntent = matchedPath ? undefined : pageIntentForPath(location.pathname);
  return (
    <aside className="sidebar" aria-label={app.t("Workspace navigation", "工作区导航")}>
      <div className="mobile-nav">
        <span>{app.t("Page", "页面")}</span>
        <select
          aria-label={app.t("Page", "页面")}
          value={activePath}
          onChange={(event) => nav(event.target.value)}
        >
          {currentIntent && (
            <option value={location.pathname}>{app.t(currentIntent.title, currentIntent.titleZh)}</option>
          )}
          {groups.filter((group) => navigation.some((item) => item.group === group)).map((group) => (
            <optgroup key={group} label={app.t(...GROUP_CAPTIONS[group])}>
              {navigation.filter((item) => item.group === group).map((item) => {
                const path = navPath(item, app.workspaceId);
                return <option key={item.key} value={path}>{app.t(item.label, item.labelZh)}</option>;
              })}
            </optgroup>
          ))}
        </select>
      </div>
      {groups.filter((group) => navigation.some((item) => item.group === group)).map((group) => (
        <div key={group} className="nav-group">
          <div className="nav-caption">{app.t(...GROUP_CAPTIONS[group])}</div>
          {navigation.filter((item) => item.group === group).map((item) => (
            <div key={item.key}>
              {item.sectionLabel && (
                <div className="nav-subcaption">
                  {app.t(item.sectionLabel, item.sectionLabelZh ?? item.sectionLabel)}
                </div>
              )}
              <SidebarItem item={item} />
            </div>
          ))}
        </div>
      ))}
    </aside>
  );
}
