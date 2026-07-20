// Navigation SSOT: router, sidebar, command palette and breadcrumbs all derive
// from this module. Tenancy is Org ▸ Workspace (ADR-0048/0051): a workspace owns
// BOTH its run resources and its config/supply, addressed under `/w/:ws/…`. The
// `:ws` segment is the active workspace; data calls carry the scope via ws().

export type Scope = "global" | "workspace";
// IA by altitude (not by object type): Author what an agent IS, from reusable
// Building blocks; Operate agents at runtime; Supply the inference layer; Observe;
// Govern. The assistant is an authoring aid (an Agents entry), not a nav peer.
export type NavGroup = "global" | "author" | "blocks" | "operate" | "supply" | "observe" | "govern";

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

  // Author: the agent is the hero object you design. (The Admin Assistant — "draft
  // with AI" — is reached from the Agents list, not the rail: it's an aid, not an object.)
  { key: "agents", label: "Agents", labelZh: "Agents", group: "author", path: "/w/:ws/agents", agentBadge: true },

  // Building blocks: the reusable resources an agent references, shared across agents.
  { key: "environments", label: "Environments", labelZh: "运行环境", group: "blocks", path: "/w/:ws/environments" },
  { key: "skills", label: "Skills", labelZh: "技能", group: "blocks", path: "/w/:ws/skills" },
  { key: "memory", label: "Memory stores", labelZh: "记忆库", group: "blocks", path: "/w/:ws/memory" },
  { key: "mcp", label: "MCP servers", labelZh: "MCP 服务", group: "blocks", path: "/w/:ws/mcp-servers" },
  { key: "a2a", label: "A2A servers", labelZh: "A2A 服务", group: "blocks", path: "/w/:ws/a2a-servers" },

  // Operate: agents put into operation. A Deployment is a standing rule (agent ×
  // environment × trigger) that PRODUCES sessions; a Session is one run instance.
  { key: "overview", label: "Overview", labelZh: "工作区概览", group: "operate", path: "/w/:ws/overview" },
  { key: "deployments", label: "Deployments", labelZh: "调度部署", group: "operate", path: "/w/:ws/deployments" },
  { key: "sessions", label: "Sessions", labelZh: "会话", group: "operate", path: "/w/:ws/sessions" },
  { key: "protocols", label: "Protocols & API", labelZh: "协议与 API", group: "operate", path: "/w/:ws/protocols" },

  // Supply: the inference layer — infrastructure, not agent-bound building blocks.
  { key: "models", label: "Models", labelZh: "模型", group: "supply", path: "/w/:ws/models" },
  { key: "credentials", label: "Inference credentials", labelZh: "推理凭证", group: "supply", path: "/w/:ws/credentials" },
  { key: "vaults", label: "Vaults", labelZh: "运行凭证", group: "supply", path: "/w/:ws/vaults" },

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
  // The assistant is an authoring aid (reached from Agents), not a rail item.
  if (pathname.match(/^\/w\/[^/]+\/assistant$/)) {
    return { scope: ws, title: "Draft with AI" };
  }
  const hit = NAV.find((n) => {
    const pattern = "^" + n.path.replace(":ws", "[^/]+") + "$";
    return new RegExp(pattern).test(pathname);
  });
  if (!hit) return { scope: "", title: "" };
  const scope = hit.group === "global" ? "Workspaces" : ws || "Workspace";
  return { scope, title: hit.label };
}
