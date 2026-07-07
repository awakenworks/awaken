// Navigation SSOT: router, sidebar, command palette and breadcrumbs all
// derive from this module (design/web-ui.md §2/§6).

export type Scope = "global" | "project" | "workspace";
export type NavGroup = "global" | "project" | "supply" | "observe" | "govern";

export interface NavItem {
  key: string;
  label: string;
  labelZh: string;
  group: NavGroup;
  /** Route path; `:pid` is substituted with the active project. */
  path: string;
  /** Marks pages whose backend face is not mounted yet (gated placeholder). */
  gated?: boolean;
  agentBadge?: boolean;
}

export const NAV: NavItem[] = [
  { key: "home", label: "Home", labelZh: "总览", group: "global", path: "/" },

  { key: "overview", label: "Overview", labelZh: "项目概览", group: "project", path: "/p/:pid/overview" },
  { key: "sessions", label: "Sessions", labelZh: "会话", group: "project", path: "/p/:pid/sessions" },
  { key: "vaults", label: "Vaults", labelZh: "运行凭证", group: "project", path: "/p/:pid/vaults" },
  { key: "bindings", label: "Agents", labelZh: "Agent 绑定", group: "project", path: "/p/:pid/agents" },
  { key: "psettings", label: "Settings", labelZh: "项目设置", group: "project", path: "/p/:pid/settings" },

  { key: "agents", label: "Agents", labelZh: "Agents", group: "supply", path: "/agents" },
  { key: "skills", label: "Skills", labelZh: "技能", group: "supply", path: "/skills", gated: true },
  { key: "tools", label: "Tools", labelZh: "工具", group: "supply", path: "/tools", gated: true },
  { key: "models", label: "Models", labelZh: "模型", group: "supply", path: "/models" },
  { key: "credentials", label: "Credentials", labelZh: "凭证", group: "supply", path: "/credentials" },
  { key: "mcp", label: "MCP servers", labelZh: "MCP 服务器", group: "supply", path: "/mcp-servers" },
  { key: "a2a", label: "A2A servers", labelZh: "A2A 服务器", group: "supply", path: "/a2a-servers" },

  { key: "dashboard", label: "Dashboard", labelZh: "看板", group: "observe", path: "/dashboard", gated: true },
  { key: "evals", label: "Eval runs", labelZh: "评测", group: "observe", path: "/eval-runs", gated: true },
  { key: "datasets", label: "Datasets", labelZh: "数据集", group: "observe", path: "/datasets", gated: true },
  { key: "audit", label: "Audit log", labelZh: "审计", group: "observe", path: "/audit-log", gated: true },

  { key: "access", label: "Access", labelZh: "访问控制", group: "govern", path: "/access" },
  { key: "wsettings", label: "Settings", labelZh: "设置", group: "govern", path: "/settings" },
];

export function navPath(item: NavItem, projectId: string): string {
  return item.path.replace(":pid", projectId || "-");
}

export function titleForPath(pathname: string): { scope: string; title: string } {
  const project = pathname.match(/^\/p\/([^/]+)\//);
  const hit = NAV.find((n) => {
    const pattern = "^" + n.path.replace(":pid", "[^/]+") + "$";
    return new RegExp(pattern).test(pathname);
  });
  if (pathname.match(/^\/p\/[^/]+\/sessions\/.+/)) {
    return { scope: project ? project[1] : "", title: "Session" };
  }
  if (pathname.match(/^\/agents\/.+/)) return { scope: "Workspace", title: "Agent editor" };
  if (!hit) return { scope: "", title: "" };
  const scope = hit.group === "global" ? "All projects" : hit.group === "project" ? (project?.[1] ?? "Project") : "Workspace";
  return { scope, title: hit.label };
}
