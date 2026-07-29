// Navigation SSOT. Workspace is the only public scope; groups follow the
// operator's job rather than internal implementation layers.

export type NavGroup = "workspace" | "build" | "run" | "supply" | "govern";

export interface NavItem {
  key: string;
  label: string;
  labelZh: string;
  group: NavGroup;
  path: string;
  agentBadge?: boolean;
}

export const NAV: NavItem[] = [
  { key: "overview", label: "Overview", labelZh: "概览", group: "workspace", path: "/w/:ws/overview" },

  { key: "agents", label: "Agents", labelZh: "Agents", group: "build", path: "/w/:ws/agents", agentBadge: true },
  { key: "skills", label: "Skills", labelZh: "技能", group: "build", path: "/w/:ws/skills" },
  { key: "memory", label: "Memory", labelZh: "记忆", group: "build", path: "/w/:ws/memory" },
  { key: "environments", label: "Environments", labelZh: "运行环境", group: "build", path: "/w/:ws/environments" },

  { key: "deployments", label: "Deployments", labelZh: "部署", group: "run", path: "/w/:ws/deployments" },
  { key: "sessions", label: "Sessions", labelZh: "会话", group: "run", path: "/w/:ws/sessions" },
  { key: "protocols", label: "API & protocols", labelZh: "API 与协议", group: "run", path: "/w/:ws/protocols" },
  { key: "a2a", label: "A2A federation", labelZh: "A2A 联邦", group: "run", path: "/w/:ws/a2a-servers" },

  { key: "models", label: "Providers & models", labelZh: "供应商与模型", group: "supply", path: "/w/:ws/models" },
  { key: "credentials", label: "Inference credentials", labelZh: "推理凭证", group: "supply", path: "/w/:ws/credentials" },

  { key: "access", label: "Access", labelZh: "访问控制", group: "govern", path: "/w/:ws/access" },
  { key: "vaults", label: "Runtime secrets", labelZh: "运行秘密", group: "govern", path: "/w/:ws/vaults" },
  { key: "settings", label: "Settings", labelZh: "设置", group: "govern", path: "/w/:ws/settings" },
];

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
