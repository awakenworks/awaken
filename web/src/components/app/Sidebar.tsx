// The left rail: a scope caption + data-driven nav (design handoff geometry —
// 224px rail, 32px raised-pill items). The org/workspace switcher moved to the
// topbar (TopChrome), so the rail is purely the active workspace's nav.

import { useLocation, useNavigate } from "react-router";
import { NAV, navPath } from "../../lib/navigation/paths";
import type { NavGroup, NavItem } from "../../lib/navigation/paths";
import { useApp } from "../../lib/app-state";

const GROUP_CAPTIONS: Record<NavGroup, [string, string]> = {
  global: ["Global", "全局"],
  run: ["Run", "运行"],
  supply: ["Supply", "供给"],
  observe: ["Observe", "观测"],
  govern: ["Govern", "治理"],
};

function SidebarItem({ item }: { item: NavItem }) {
  const app = useApp();
  const nav = useNavigate();
  const location = useLocation();
  const concrete = navPath(item, app.workspaceId);
  const active = concrete === "/" ? location.pathname === "/" : location.pathname.startsWith(concrete);
  return (
    <button
      className="nav-item"
      data-active={active}
      onClick={() => nav(concrete)}
      title={item.gated ? app.t("Backend face not mounted yet", "后端面尚未就绪") : undefined}
    >
      {app.t(item.label, item.labelZh)}
      {item.gated && <span className="mut" style={{ marginLeft: "auto", fontSize: 10 }}>◌</span>}
    </button>
  );
}

export default function Sidebar() {
  const app = useApp();
  const activeWs = app.workspaces.find((w) => w.id === app.workspaceId);
  const groups: NavGroup[] = ["run", "supply", "observe", "govern"];

  return (
    <aside className="sidebar">
      <div className="nav-caption">
        {app.t("Workspace", "工作区")} · {activeWs?.display_name || app.workspaceId}
      </div>

      <div className="nav-group">
        {NAV.filter((n) => n.group === "global").map((n) => (
          <SidebarItem key={n.key} item={n} />
        ))}
      </div>

      {groups.map((g) => (
        <div key={g} className="nav-group">
          <div className="nav-caption">{app.t(...GROUP_CAPTIONS[g])}</div>
          {NAV.filter((n) => n.group === g).map((n) => (
            <SidebarItem key={n.key} item={n} />
          ))}
        </div>
      ))}
    </aside>
  );
}
