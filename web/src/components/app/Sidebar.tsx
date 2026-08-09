import { useLocation, useNavigate } from "react-router";
import { navPath, visibleNavigation, type NavGroup, type NavItem } from "../../lib/navigation/paths";
import { useApp } from "../../lib/app-state";
import { useConfigCapabilities } from "../../lib/useConfigCapabilities";

const GROUP_CAPTIONS: Record<NavGroup, [string, string]> = {
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
  const capabilities = useConfigCapabilities();
  const byokEnabled = capabilities.data?.models.byok_enabled === true;
  const navigation = visibleNavigation(byokEnabled);
  const groups: NavGroup[] = ["workspace", "author", "run", "connect", "govern"];
  return (
    <aside className="sidebar" aria-label={app.t("Workspace navigation", "工作区导航")}>
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
