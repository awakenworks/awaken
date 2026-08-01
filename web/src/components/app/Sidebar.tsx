import { useLocation, useNavigate } from "react-router";
import { navPath, visibleNavigation, type NavGroup, type NavItem } from "../../lib/navigation/paths";
import { useApp, workspaceLabel } from "../../lib/app-state";
import { useConfigCapabilities } from "../../lib/useConfigCapabilities";

const GROUP_CAPTIONS: Record<NavGroup, [string, string]> = {
  workspace: ["Workspace", "工作区"],
  build: ["Build", "构建"],
  run: ["Run", "运行"],
  supply: ["AI supply", "AI 供给"],
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
  const groups: NavGroup[] = ["workspace", "build", "run", "supply", "govern"];
  return (
    <aside className="sidebar">
      <div className="nav-caption" title={app.workspaceId}>
        {app.t("Workspace", "工作区")} · {workspaceLabel(app.workspaceId)}
      </div>
      {groups.filter((group) => navigation.some((item) => item.group === group)).map((group) => (
        <div key={group} className="nav-group">
          <div className="nav-caption">{app.t(...GROUP_CAPTIONS[group])}</div>
          {navigation.filter((item) => item.group === group).map((item) => (
            <SidebarItem key={item.key} item={item} />
          ))}
        </div>
      ))}
    </aside>
  );
}
