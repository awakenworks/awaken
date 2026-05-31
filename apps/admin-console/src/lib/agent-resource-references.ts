import type { AgentSpec } from "./config-api";

export const SKILLS_SECTION_KEY = "skills";
export const SKILLS_DISCOVERY_SECTION_KEY = "skills-discovery";
export const SKILLS_DISCOVERY_PLUGIN_ID = "skills-discovery";
export const SKILLS_ACTIVE_PLUGIN_ID = "skills-active-instructions";

function isRecord(value: unknown): value is Record<string, unknown> {
  return value !== null && typeof value === "object" && !Array.isArray(value);
}

function readStringArray(value: unknown): string[] | null {
  if (!Array.isArray(value)) return null;
  return value.filter((item): item is string => typeof item === "string");
}

export function readSkillAllowlist(spec: AgentSpec): string[] | null {
  const sections = spec.sections ?? {};
  const skills = isRecord(sections[SKILLS_SECTION_KEY]) ? sections[SKILLS_SECTION_KEY] : null;
  const allowlist = readStringArray(skills?.allowlist);
  if (allowlist) return allowlist;

  const legacy = isRecord(sections[SKILLS_DISCOVERY_SECTION_KEY])
    ? sections[SKILLS_DISCOVERY_SECTION_KEY]
    : null;
  return readStringArray(legacy?.ids);
}

export function withSkillAllowlist(spec: AgentSpec, allowlist: string[] | null) {
  const sections: Record<string, unknown> = { ...(spec.sections ?? {}) };
  const current = isRecord(sections[SKILLS_SECTION_KEY])
    ? { ...sections[SKILLS_SECTION_KEY] }
    : {};

  if (allowlist === null) {
    delete current.allowlist;
  } else {
    current.allowlist = allowlist;
  }

  if (Object.keys(current).length === 0) {
    delete sections[SKILLS_SECTION_KEY];
  } else {
    sections[SKILLS_SECTION_KEY] = current;
  }

  return sections;
}

export function mcpServerToolPrefix(serverId: string): string {
  return `mcp__${serverId}__`;
}

export function mcpServerPattern(serverId: string): string {
  return `${mcpServerToolPrefix(serverId)}*`;
}

export function selectedMcpServerIds(
  spec: AgentSpec,
  knownServerIds: readonly string[] = [],
): string[] {
  const ids = new Set<string>();
  for (const pattern of spec.allowed_tool_patterns ?? []) {
    const match = /^mcp__(.+)__\*$/.exec(pattern);
    if (match) ids.add(match[1]);
  }
  for (const toolId of spec.allowed_tools ?? []) {
    for (const serverId of knownServerIds) {
      if (toolId.startsWith(mcpServerToolPrefix(serverId))) ids.add(serverId);
    }
  }
  return [...ids].sort();
}

export function agentMentionsMcpServer(agent: AgentSpec, mcpId: string): boolean {
  const sections = agent.sections ?? {};
  const mcpSection = isRecord(sections.mcp) ? sections.mcp : null;
  const servers = Array.isArray(mcpSection?.servers) ? mcpSection.servers : [];
  for (const server of servers) {
    if (typeof server === "string" && server === mcpId) return true;
    if (isRecord(server) && server.id === mcpId) return true;
  }

  if ((agent.plugin_ids ?? []).includes(mcpId)) return true;

  const prefix = mcpServerToolPrefix(mcpId);
  return (
    (agent.allowed_tools ?? []).some((toolId) => toolId.startsWith(prefix)) ||
    (agent.excluded_tools ?? []).some((toolId) => toolId.startsWith(prefix)) ||
    (agent.allowed_tool_patterns ?? []).includes(mcpServerPattern(mcpId)) ||
    (agent.excluded_tool_patterns ?? []).includes(mcpServerPattern(mcpId))
  );
}
