// Navigation SSOT. Workspace is the only public scope; groups follow the
// operator's job rather than internal implementation layers.

import type { ConfigCapabilitiesView } from "../api/types";

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
  surface?: keyof ConfigCapabilitiesView["surfaces"];
}

export const NAV: NavItem[] = [
  { key: "overview", label: "Overview", labelZh: "概览", group: "workspace", path: "/w/:ws/overview" },

  { key: "agents", label: "Agents", labelZh: "Agents", group: "author", path: "/w/:ws/agents", agentBadge: true },
  { key: "skills", label: "Skills", labelZh: "技能", group: "author", path: "/w/:ws/skills", surface: "managed_runtime" },
  {
    key: "files",
    label: "Files",
    labelZh: "文件",
    group: "author",
    path: "/w/:ws/files",
    sectionLabel: "Resources",
    sectionLabelZh: "资源",
    surface: "managed_runtime",
  },
  { key: "memory", label: "Memory", labelZh: "记忆", group: "author", path: "/w/:ws/memory", surface: "managed_runtime" },

  { key: "sessions", label: "Sessions", labelZh: "会话", group: "run", path: "/w/:ws/sessions", surface: "managed_runtime" },
  { key: "deployments", label: "Deployments", labelZh: "部署", group: "run", path: "/w/:ws/deployments", surface: "managed_runtime" },
  { key: "artifacts", label: "Artifacts", labelZh: "产物", group: "run", path: "/w/:ws/artifacts", surface: "managed_runtime" },
  { key: "environments", label: "Environments", labelZh: "运行环境", group: "run", path: "/w/:ws/environments", surface: "managed_runtime" },

  { key: "models", label: "Models & providers", labelZh: "模型与供应商", group: "connect", path: "/w/:ws/models" },
  { key: "mcp", label: "MCP overview", labelZh: "MCP 概览", group: "connect", path: "/w/:ws/mcp", surface: "managed_runtime" },
  { key: "protocols", label: "API & protocols", labelZh: "API 与协议", group: "connect", path: "/w/:ws/protocols", surface: "managed_runtime" },
  { key: "webhooks", label: "Webhooks", labelZh: "Webhooks", group: "connect", path: "/w/:ws/webhooks", surface: "managed_runtime" },
  { key: "a2a", label: "A2A federation", labelZh: "A2A 联邦", group: "connect", path: "/w/:ws/a2a-servers", surface: "managed_runtime" },

  { key: "access", label: "Access", labelZh: "访问控制", group: "govern", path: "/w/:ws/access", surface: "access_management" },
  { key: "vaults", label: "Runtime secrets", labelZh: "运行时凭证", group: "govern", path: "/w/:ws/vaults", surface: "managed_runtime" },
  { key: "settings", label: "Settings", labelZh: "设置", group: "govern", path: "/w/:ws/settings" },
];

export interface WorkspaceJourneyStep {
  readonly number: string;
  readonly label: string;
  readonly labelZh: string;
  readonly detail: string;
  readonly detailZh: string;
  readonly destination: NavItem;
}

function primarySurface(key: string): NavItem {
  const item = NAV.find((candidate) => candidate.key === key);
  if (!item) throw new Error(`Unknown primary surface: ${key}`);
  return item;
}

/** A presentational journey over NAV references. NAV remains the sole route
 * authority; the overview only explains the order in which operators prove an
 * Agent, and never copies a path or creates workflow state. */
export const WORKSPACE_JOURNEY: readonly WorkspaceJourneyStep[] = [
  { number: "01", label: "Connect", labelZh: "连接", detail: "Model supply and runtime capabilities", detailZh: "模型供给与运行能力", destination: primarySurface("models") },
  { number: "02", label: "Build", labelZh: "构建", detail: "One publishable Agent definition", detailZh: "一个可发布的 Agent 定义", destination: primarySurface("agents") },
  { number: "03", label: "Run", labelZh: "运行", detail: "A Session on the reviewed Agent version", detailZh: "基于已审阅 Agent 版本的 Session", destination: primarySurface("sessions") },
  { number: "04", label: "Observe", labelZh: "观察", detail: "Committed events, artifacts, and usage", detailZh: "已提交事件、产物与用量", destination: primarySurface("artifacts") },
  { number: "05", label: "Integrate", labelZh: "接入", detail: "API key, protocol, and runnable SDK guide", detailZh: "API Key、协议与可运行 SDK 指南", destination: primarySurface("protocols") },
];

/** Capability-derived navigation projection. The backend remains the security
 * boundary. The current information architecture already has no parallel
 * credential navigation item, so deployment posture changes only the canonical
 * model-supply label and never creates a second route registry. */
export function hasSurface(
  capabilities: ConfigCapabilitiesView | undefined,
  surface: keyof ConfigCapabilitiesView["surfaces"],
): boolean {
  // During a rolling image replacement, an older backend can briefly serve a
  // newer static bundle. Missing discovery fails closed instead of crashing or
  // guessing that an unmounted API exists.
  return capabilities?.surfaces?.[surface] === true;
}

export function visibleNavigation(capabilities: ConfigCapabilitiesView | undefined): NavItem[] {
  const byokEnabled = capabilities?.models.byok_enabled === true;
  return NAV.filter((item) => !item.surface || hasSurface(capabilities, item.surface)).map((item) =>
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
  if (pathname.match(/^\/w\/[^/]+\/memory\/dreams\/.+/)) return { scope: ws, title: "Dream" };
  if (pathname.match(/^\/w\/[^/]+\/assistant$/)) return { scope: ws, title: "Assistant" };
  const hit = NAV.find((item) =>
    new RegExp(`^${item.path.replace(":ws", "[^/]+")}$`).test(pathname),
  );
  return hit ? { scope: ws || "Workspace", title: hit.label } : { scope: "", title: "" };
}
