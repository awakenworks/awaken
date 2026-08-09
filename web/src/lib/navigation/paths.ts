// Navigation SSOT. Workspace is the only public scope; groups follow the
// operator's job rather than internal implementation layers.

export type NavGroup = "workspace" | "author" | "run" | "connect" | "govern";

export interface NavItem {
  key: string;
  label: string;
  labelZh: string;
  group: NavGroup;
  path: string;
  agentBadge?: boolean;
  sectionLabel?: string;
  sectionLabelZh?: string;
}

export const NAV: NavItem[] = [
  { key: "overview", label: "Overview", labelZh: "概览", group: "workspace", path: "/w/:ws/overview" },

  { key: "agents", label: "Agents", labelZh: "Agents", group: "author", path: "/w/:ws/agents", agentBadge: true },
  { key: "skills", label: "Skills", labelZh: "技能", group: "author", path: "/w/:ws/skills" },
  {
    key: "files",
    label: "Files",
    labelZh: "文件",
    group: "author",
    path: "/w/:ws/files",
    sectionLabel: "Resources",
    sectionLabelZh: "资源",
  },
  { key: "memory", label: "Memory stores", labelZh: "记忆库", group: "author", path: "/w/:ws/memory" },

  { key: "sessions", label: "Sessions", labelZh: "会话", group: "run", path: "/w/:ws/sessions" },
  { key: "deployments", label: "Deployments", labelZh: "部署", group: "run", path: "/w/:ws/deployments" },
  { key: "artifacts", label: "Artifacts", labelZh: "产物", group: "run", path: "/w/:ws/artifacts" },
  { key: "environments", label: "Environments", labelZh: "运行环境", group: "run", path: "/w/:ws/environments" },

  { key: "models", label: "Models & providers", labelZh: "模型与供应商", group: "connect", path: "/w/:ws/models" },
  { key: "mcp", label: "MCP overview", labelZh: "MCP 概览", group: "connect", path: "/w/:ws/mcp" },
  { key: "protocols", label: "API & protocols", labelZh: "API 与协议", group: "connect", path: "/w/:ws/protocols" },
  { key: "a2a", label: "A2A federation", labelZh: "A2A 联邦", group: "connect", path: "/w/:ws/a2a-servers" },

  { key: "access", label: "Access", labelZh: "访问控制", group: "govern", path: "/w/:ws/access" },
  { key: "vaults", label: "Runtime secrets", labelZh: "运行秘密", group: "govern", path: "/w/:ws/vaults" },
  { key: "settings", label: "Settings", labelZh: "设置", group: "govern", path: "/w/:ws/settings" },
];

/** Capability-derived navigation projection. The backend remains the security
 * boundary; this removes deployment-inapplicable tasks without creating a
 * second Cloud/local mode flag in the browser. */
export function visibleNavigation(byokEnabled: boolean): NavItem[] {
  return NAV.map((item) =>
    !byokEnabled && item.key === "models"
      ? { ...item, label: "Models", labelZh: "模型", group: "author" }
      : item,
  );
}

export function navPath(item: NavItem, workspaceId: string): string {
  return item.path.replace(":ws", workspaceId || "default");
}

export function titleForPath(pathname: string): { scope: string; title: string } {
  const ws = pathname.match(/^\/w\/([^/]+)\//)?.[1] ?? "";
  if (pathname.match(/^\/w\/[^/]+\/sessions\/.+/)) return { scope: ws, title: "Session" };
  if (pathname.match(/^\/w\/[^/]+\/agents\/.+/)) return { scope: ws, title: "Agent" };
  if (pathname.match(/^\/w\/[^/]+\/assistant$/)) return { scope: ws, title: "Draft with AI" };
  const hit = NAV.find((item) =>
    new RegExp(`^${item.path.replace(":ws", "[^/]+")}$`).test(pathname),
  );
  return hit ? { scope: ws || "Workspace", title: hit.label } : { scope: "", title: "" };
}
