// Navigation SSOT: router, sidebar, command palette and breadcrumbs all derive
// from this module. Tenancy is Org ▸ Workspace (ADR-0048/0051): a workspace owns
// BOTH its run resources and its config/supply, addressed under `/w/:ws/…`. The
// `:ws` segment is the active workspace; data calls carry the scope via ws().

export type Scope = "global" | "workspace";
export type NavGroup = "global" | "run" | "supply" | "observe" | "govern";

export interface NavItem {
  key: string;
  label: string;
  labelZh: string;
  group: NavGroup;
  /** Route path; `:ws` is substituted with the active workspace. */
  path: string;
  /** Marks pages whose backend face is not mounted yet (gated placeholder). */
  gated?: boolean;
  agentBadge?: boolean;
}

export const NAV: NavItem[] = [
  { key: "home", label: "Home", labelZh: "总览", group: "global", path: "/" },

  // Run: the workspace's Managed Agents runtime — sessions, agents, environments,
  // vaults, memory stores, deployments, skills.
  { key: "overview", label: "Overview", labelZh: "工作区概览", group: "run", path: "/w/:ws/overview" },
  { key: "sessions", label: "Sessions", labelZh: "会话", group: "run", path: "/w/:ws/sessions" },
  { key: "agents", label: "Agents", labelZh: "Agents", group: "run", path: "/w/:ws/agents" },
  { key: "environments", label: "Environments", labelZh: "运行环境", group: "run", path: "/w/:ws/environments" },
  { key: "vaults", label: "Vaults", labelZh: "运行凭证", group: "run", path: "/w/:ws/vaults" },
  { key: "memory", label: "Memory stores", labelZh: "记忆库", group: "run", path: "/w/:ws/memory" },
  { key: "deployments", label: "Deployments", labelZh: "调度部署", group: "run", path: "/w/:ws/deployments" },
  { key: "skills", label: "Skills", labelZh: "技能", group: "run", path: "/w/:ws/skills" },

  // Supply: the workspace's shared config the run plane references by id.
  { key: "models", label: "Models", labelZh: "模型", group: "supply", path: "/w/:ws/models" },
  { key: "credentials", label: "Inference credentials", labelZh: "推理凭证", group: "supply", path: "/w/:ws/credentials" },
  { key: "mcp", label: "MCP catalog", labelZh: "MCP 目录", group: "supply", path: "/w/:ws/mcp-servers" },
  { key: "a2a", label: "A2A catalog", labelZh: "A2A 目录", group: "supply", path: "/w/:ws/a2a-servers" },

  { key: "dashboard", label: "Dashboard", labelZh: "看板", group: "observe", path: "/w/:ws/dashboard", gated: true },
  { key: "evals", label: "Eval runs", labelZh: "评测", group: "observe", path: "/w/:ws/eval-runs", gated: true },
  { key: "datasets", label: "Datasets", labelZh: "数据集", group: "observe", path: "/w/:ws/datasets", gated: true },
  { key: "audit", label: "Audit log", labelZh: "审计", group: "observe", path: "/w/:ws/audit-log", gated: true },

  { key: "access", label: "Access", labelZh: "访问控制", group: "govern", path: "/w/:ws/access" },
  { key: "settings", label: "Settings", labelZh: "工作区设置", group: "govern", path: "/w/:ws/settings" },
];

export function navPath(item: NavItem, workspaceId: string): string {
  return item.path.replace(":ws", workspaceId || "default");
}

export function titleForPath(pathname: string): { scope: string; title: string } {
  const wsMatch = pathname.match(/^\/w\/([^/]+)\//);
  const ws = wsMatch ? wsMatch[1] : "";
  if (pathname.match(/^\/w\/[^/]+\/sessions\/.+/)) {
    return { scope: ws, title: "Session" };
  }
  if (pathname.match(/^\/w\/[^/]+\/agents\/.+/)) {
    return { scope: ws, title: "Agent" };
  }
  const hit = NAV.find((n) => {
    const pattern = "^" + n.path.replace(":ws", "[^/]+") + "$";
    return new RegExp(pattern).test(pathname);
  });
  if (!hit) return { scope: "", title: "" };
  const scope = hit.group === "global" ? "Workspaces" : ws || "Workspace";
  return { scope, title: hit.label };
}
