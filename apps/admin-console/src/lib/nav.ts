import { adminRoutes } from "./routes";

export type NavBadge = "live" | "ro";
export type NavHealthSource = "mcp" | "providers";

export interface NavItem {
  id: string;
  path: string;
  label: string;
  end?: boolean;
  badge?: NavBadge;
  healthSource?: NavHealthSource;
}

export interface NavGroup {
  label: string;
  items: NavItem[];
}

export const navGroups: NavGroup[] = [
  {
    label: "Configure",
    items: [
      { id: "agents", path: adminRoutes.agents, label: "Agents" },
      { id: "models", path: adminRoutes.models, label: "Models" },
      {
        id: "providers",
        path: adminRoutes.providers,
        label: "Providers",
        healthSource: "providers",
      },
      {
        id: "mcp-servers",
        path: adminRoutes.mcpServers,
        label: "MCP Servers",
        healthSource: "mcp",
      },
    ],
  },
  {
    label: "Observe",
    items: [
      { id: "dashboard", path: adminRoutes.dashboard, label: "Dashboard", end: true },
      { id: "audit-log", path: adminRoutes.auditLog, label: "Audit Log" },
      { id: "eval-reports", path: adminRoutes.evalReports, label: "Eval Reports" },
      { id: "skills", path: adminRoutes.skills, label: "Skill Registry", badge: "ro" },
    ],
  },
  {
    label: "Assistant",
    items: [{ id: "assistant", path: adminRoutes.assistant, label: "AI Assistant" }],
  },
];

export const navIndex: Record<string, NavItem & { group: string }> = {};
for (const group of navGroups) {
  for (const item of group.items) {
    navIndex[item.path] = { ...item, group: group.label };
  }
}

export interface BreadcrumbCrumb {
  label: string;
  path?: string;
}

/** Resolve a route pathname into a breadcrumb chain (Group / Page). */
export function resolveBreadcrumbs(pathname: string): BreadcrumbCrumb[] {
  if (pathname === adminRoutes.dashboard) {
    return [{ label: "Observe" }, { label: "Dashboard" }];
  }
  if (pathname === adminRoutes.agentNew) {
    return [
      { label: "Configure" },
      { label: "Agents", path: adminRoutes.agents },
      { label: "New" },
    ];
  }
  const agentMatch = pathname.match(/^\/agents\/([^/]+)(?:\/(dashboard))?$/);
  if (agentMatch) {
    const id = decodeURIComponent(agentMatch[1]);
    const isDashboard = Boolean(agentMatch[2]);
    return [
      { label: "Configure" },
      { label: "Agents", path: adminRoutes.agents },
      isDashboard
        ? { label: id, path: adminRoutes.agent(id) }
        : { label: id },
      ...(isDashboard ? [{ label: "Dashboard" }] : []),
    ];
  }
  const top = navIndex[pathname];
  if (top) return [{ label: top.group }, { label: top.label }];
  return [{ label: "Admin" }];
}
