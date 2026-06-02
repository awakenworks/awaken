import { adminRoutes } from "./routes";

export type NavBadge = "live" | "ro";
export type NavHealthSource = "mcp" | "providers";

export interface NavItem {
  id: string;
  path: string;
  label: string;
  /** i18n translation key for `label`. Render-side calls t(labelKey) when present. */
  labelKey?: string;
  end?: boolean;
  badge?: NavBadge;
  healthSource?: NavHealthSource;
}

export interface NavGroup {
  label: string;
  items: NavItem[];
}

/**
 * IA v2.4 — sidebar grouped by topology layer, not by verb.
 *   Agents (consumer) → Resources (runtime deps: mcp · skills · tools) →
 *   Infrastructure (providers · models) → Observe (lenses).
 *
 * `label` and `groupKey` are i18n translation keys, not literal display text.
 * Render-side resolves them via t().
 */
export interface NavGroupKeyed extends NavGroup {
  groupKey: string;
}

export interface NavItemKeyed extends NavItem {
  labelKey: string;
}

export const navGroups: (NavGroup & { groupKey: string })[] = [
  {
    groupKey: "nav.agents",
    label: "Agents",
    items: [
      { id: "agents", path: adminRoutes.agents, label: "Agents", labelKey: "nav.items.agents" } as NavItemKeyed,
    ] as NavItem[],
  },
  {
    groupKey: "nav.resources",
    label: "Resources",
    items: [
      { id: "a2a-servers", path: adminRoutes.a2aServers, label: "A2A Servers", labelKey: "nav.items.a2a" } as NavItemKeyed,
      { id: "mcp-servers", path: adminRoutes.mcpServers, label: "MCP Servers", healthSource: "mcp", labelKey: "nav.items.mcp" } as NavItemKeyed,
      { id: "skills", path: adminRoutes.skills, label: "Skills", badge: "ro", labelKey: "nav.items.skills" } as NavItemKeyed,
      { id: "tools", path: adminRoutes.tools, label: "Tools", labelKey: "nav.items.tools" } as NavItemKeyed,
    ] as NavItem[],
  },
  {
    groupKey: "nav.infrastructure",
    label: "Infrastructure",
    items: [
      { id: "providers", path: adminRoutes.providers, label: "Providers", healthSource: "providers", labelKey: "nav.items.providers" } as NavItemKeyed,
      { id: "models", path: adminRoutes.models, label: "Models", labelKey: "nav.items.models" } as NavItemKeyed,
    ] as NavItem[],
  },
  {
    groupKey: "nav.observe",
    label: "Observe",
    items: [
      { id: "dashboard", path: adminRoutes.dashboard, label: "Dashboard", end: true, labelKey: "nav.items.dashboard" } as NavItemKeyed,
      { id: "audit-log", path: adminRoutes.auditLog, label: "Audit Log", labelKey: "nav.items.audit" } as NavItemKeyed,
      { id: "datasets", path: adminRoutes.datasets, label: "Datasets", labelKey: "nav.items.datasets" } as NavItemKeyed,
      { id: "eval-runs", path: adminRoutes.evalRuns, label: "Eval Runs", labelKey: "nav.items.evalRuns" } as NavItemKeyed,
      { id: "eval-reports", path: adminRoutes.evalReports, label: "Eval Reports", labelKey: "nav.items.evals" } as NavItemKeyed,
    ] as NavItem[],
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
  /** i18n translation key for `label`. Render-side should call t(labelKey) when present. */
  labelKey?: string;
  path?: string;
}

const navIndexKeyed = navIndex as Record<string, NavItem & { group: string; labelKey?: string }>;

function groupLabelByGroupName(name: string): string {
  for (const g of navGroups) {
    if (g.label === name) return g.groupKey;
  }
  return name;
}

/** Resolve a route pathname into a breadcrumb chain (Group / Page). */
export function resolveBreadcrumbs(pathname: string): BreadcrumbCrumb[] {
  if (pathname === adminRoutes.dashboard) {
    return [{ label: "Observe", labelKey: "nav.observe" }, { label: "Dashboard", labelKey: "nav.items.dashboard" }];
  }
  if (pathname === adminRoutes.agentNew) {
    return [
      { label: "Agents", labelKey: "nav.items.agents", path: adminRoutes.agents },
      { label: "New", labelKey: "common.new" },
    ];
  }
  if (pathname === adminRoutes.assistant) {
    return [
      { label: "Assistant", labelKey: "nav.assistant" },
      { label: "AI Assistant", labelKey: "nav.items.chat" },
    ];
  }
  const agentMatch = pathname.match(/^\/agents\/([^/]+)(?:\/(dashboard))?$/);
  if (agentMatch) {
    const id = decodeURIComponent(agentMatch[1]);
    const isDashboard = Boolean(agentMatch[2]);
    return [
      { label: "Agents", labelKey: "nav.items.agents", path: adminRoutes.agents },
      isDashboard
        ? { label: id, path: adminRoutes.agent(id) }
        : { label: id },
      ...(isDashboard ? [{ label: "Dashboard", labelKey: "nav.items.dashboard" }] : []),
    ];
  }
  const mcpMatch = pathname.match(/^\/mcp-servers\/([^/]+)$/);
  if (mcpMatch) {
    return [
      { label: "Resources", labelKey: "nav.resources" },
      { label: "MCP Servers", labelKey: "nav.items.mcp", path: adminRoutes.mcpServers },
      { label: decodeURIComponent(mcpMatch[1]) },
    ];
  }
  const a2aMatch = pathname.match(/^\/a2a-servers\/([^/]+)$/);
  if (a2aMatch) {
    return [
      { label: "Resources", labelKey: "nav.resources" },
      { label: "A2A Servers", labelKey: "nav.items.a2a", path: adminRoutes.a2aServers },
      { label: decodeURIComponent(a2aMatch[1]) },
    ];
  }
  const skillMatch = pathname.match(/^\/skills\/([^/]+)$/);
  if (skillMatch) {
    return [
      { label: "Resources", labelKey: "nav.resources" },
      { label: "Skills", labelKey: "nav.items.skills", path: adminRoutes.skills },
      { label: decodeURIComponent(skillMatch[1]) },
    ];
  }
  const datasetMatch = pathname.match(/^\/datasets\/([^/]+)$/);
  if (datasetMatch) {
    return [
      { label: "Observe", labelKey: "nav.observe" },
      { label: "Datasets", labelKey: "nav.items.datasets", path: adminRoutes.datasets },
      { label: decodeURIComponent(datasetMatch[1]) },
    ];
  }
  const evalRunMatch = pathname.match(/^\/eval-runs\/([^/]+)$/);
  if (evalRunMatch) {
    return [
      { label: "Observe", labelKey: "nav.observe" },
      { label: "Eval Runs", labelKey: "nav.items.evalRuns", path: adminRoutes.evalRuns },
      { label: decodeURIComponent(evalRunMatch[1]) },
    ];
  }
  const top = navIndexKeyed[pathname];
  if (top) {
    if (top.group === top.label) return [{ label: top.label, labelKey: top.labelKey }];
    return [
      { label: top.group, labelKey: groupLabelByGroupName(top.group) },
      { label: top.label, labelKey: top.labelKey },
    ];
  }
  return [{ label: "Admin" }];
}
