import type {
  AgentConfig,
  AgentManagedToolset,
  AgentMcpServer,
  AgentMcpToolset,
  AgentTool,
  AgentToolDefaultConfig,
  AgentToolPermissionPolicy,
  AgentToolsetConfig,
  AgentToolsetMemberCap,
  ManagedToolsetCap,
  McpToolsetCap,
} from "./api/types";

export type ToolPermission = AgentToolPermissionPolicy["type"];

export interface McpToolsetPolicySummary {
  enabled: boolean;
  permission: ToolPermission;
  namedOverrides: number;
}

interface McpToolsetPolicyInput {
  type: string;
  mcp_server_name?: string;
  default_config?: {
    enabled?: boolean | null;
    permission_policy?: { type?: string | null } | null;
  } | null;
  configs?: readonly unknown[] | null;
}

const ALWAYS_ALLOW: AgentToolPermissionPolicy = { type: "always_allow" };
const ALWAYS_ASK: AgentToolPermissionPolicy = { type: "always_ask" };

export function isAgentToolset(tool: AgentTool): tool is AgentManagedToolset {
  return typeof tool === "object"
    && tool !== null
    && tool.type === "agent_toolset_20260401";
}

export function isMcpToolset(tool: AgentTool): tool is AgentMcpToolset {
  return typeof tool === "object"
    && tool !== null
    && tool.type === "mcp_toolset"
    && typeof tool.mcp_server_name === "string";
}

/** Read the effective policy shown beside an MCP connection. Older stored
 * Agents may not contain a typed ToolSet yet, so the read projection keeps the
 * same fail-closed fallback as new authoring. */
export function mcpToolsetPolicySummary(
  tools: readonly (McpToolsetPolicyInput | string)[],
  serverName: string,
): McpToolsetPolicySummary {
  const toolset = tools.find((candidate): candidate is McpToolsetPolicyInput =>
    typeof candidate === "object"
    && candidate !== null
    && candidate.type === "mcp_toolset"
    && candidate.mcp_server_name === serverName);
  const requestedPermission = toolset?.default_config?.permission_policy?.type;
  return {
    enabled: toolset?.default_config?.enabled !== false,
    permission: requestedPermission === "always_allow" ? "always_allow" : "always_ask",
    namedOverrides: toolset?.configs?.length ?? 0,
  };
}

export function mcpDefaultConfig(
  capabilities: readonly ManagedToolsetCap[] | undefined,
): AgentToolDefaultConfig {
  const advertised = capabilities
    ?.find((capability): capability is McpToolsetCap => capability.type === "mcp_toolset")
    ?.default_config;
  return advertised
    ? { enabled: advertised.enabled, permission_policy: { ...advertised.permission_policy } }
    : { enabled: true, permission_policy: { type: "always_ask" } };
}

export function reconcileMcpToolsets(
  tools: readonly AgentTool[],
  servers: readonly AgentMcpServer[],
  defaultConfig: AgentToolDefaultConfig,
): AgentTool[] {
  const ordinary = tools.filter((tool) => !isMcpToolset(tool));
  const existing = new Map(
    tools.filter(isMcpToolset).map((toolset) => [toolset.mcp_server_name, toolset]),
  );
  const policies = servers.map((server): AgentMcpToolset => existing.get(server.name) ?? {
    type: "mcp_toolset",
    mcp_server_name: server.name,
    configs: [],
    default_config: defaultConfig,
  });
  return [...ordinary, ...policies];
}

export function renameMcpToolset(
  tools: readonly AgentTool[],
  previous: string,
  next: string,
): AgentTool[] {
  return tools.map((tool) => isMcpToolset(tool) && tool.mcp_server_name === previous
    ? { ...tool, mcp_server_name: next }
    : tool);
}

function renameMcpToolId(toolId: string, previous: string, next: string): string {
  const prefix = `mcp__${previous}__`;
  return toolId.startsWith(prefix) ? `mcp__${next}__${toolId.slice(prefix.length)}` : toolId;
}

export function renameMcpIntegrationReferences(
  config: AgentConfig,
  previous: string,
  next: string,
): Partial<AgentConfig> {
  return {
    tools: renameMcpToolset(config.tools, previous, next),
    tool_overrides: (config.tool_overrides ?? []).map((override) => ({
      ...override,
      target: renameMcpToolId(override.target, previous, next),
    })),
    recovery_policies: Object.fromEntries(Object.entries(config.recovery_policies ?? {}).map(
      ([toolId, policy]) => [renameMcpToolId(toolId, previous, next), policy],
    )),
    tool_exposure: config.tool_exposure ? {
      ...config.tool_exposure,
      rules: config.tool_exposure.rules?.map((rule) => ({
        ...rule,
        selector: { ...rule.selector, value: renameMcpToolId(rule.selector.value, previous, next) },
      })),
    } : undefined,
  };
}

export function removeMcpIntegration(
  config: AgentConfig,
  serverName: string,
): Partial<AgentConfig> {
  const prefix = `mcp__${serverName}__`;
  const recovery = Object.fromEntries(
    Object.entries(config.recovery_policies ?? {}).filter(([toolId]) => !toolId.startsWith(prefix)),
  );
  return {
    mcp_servers: config.mcp_servers.filter((server) => server.name !== serverName),
    tools: config.tools.filter((tool) => !isMcpToolset(tool) || tool.mcp_server_name !== serverName),
    tool_overrides: (config.tool_overrides ?? []).filter((override) => !override.target.startsWith(prefix)),
    recovery_policies: recovery,
    tool_exposure: config.tool_exposure ? {
      ...config.tool_exposure,
      rules: config.tool_exposure.rules?.filter((rule) => !rule.selector.value.startsWith(prefix)),
    } : undefined,
  };
}

export function mcpIntegrationsValid(config: AgentConfig): boolean {
  const names = config.mcp_servers.map((server) => server.name.trim());
  if (names.some((name) => name.length === 0) || new Set(names).size !== names.length) return false;
  if (config.mcp_servers.some((server) => server.type === "sandbox_stdio"
    ? server.command.trim().length === 0
    : server.url.trim().length === 0)) return false;
  const policyNames = config.tools.filter(isMcpToolset).map((toolset) => toolset.mcp_server_name);
  return policyNames.length === names.length
    && new Set(policyNames).size === policyNames.length
    && names.every((name) => policyNames.includes(name));
}

/** Capability-owned names used by both controlled authoring and its UI description. */
export function controlledModificationMemberNames(
  members: readonly AgentToolsetMemberCap[],
): string[] {
  return members
    .filter((member) => member.controlled_modification)
    .map((member) => member.name);
}

function effectiveEnabled(toolset: AgentManagedToolset, member: string): boolean {
  return toolset.configs?.find((config) => config.name === member)?.enabled
    ?? toolset.default_config?.enabled
    ?? true;
}

function effectivePermission(
  toolset: AgentManagedToolset | undefined,
  config: AgentToolsetConfig | undefined,
): AgentToolPermissionPolicy {
  return config?.permission_policy
    ?? toolset?.default_config?.permission_policy
    ?? ALWAYS_ALLOW;
}

/** Project the mixed wire union into the ids consumed by the existing picker. */
export function selectedAgentToolIds(
  tools: readonly AgentTool[],
  members: readonly AgentToolsetMemberCap[],
): string[] {
  const memberNames = members.map((member) => member.name);
  const memberSet = new Set(memberNames);
  const exact = tools.filter((tool): tool is string => typeof tool === "string");
  const selected = new Set(exact.filter((id) => memberSet.has(id)));
  for (const toolset of tools.filter(isAgentToolset)) {
    for (const member of memberNames) {
      if (effectiveEnabled(toolset, member)) selected.add(member);
    }
  }
  return [
    ...memberNames.filter((member) => selected.has(member)),
    ...exact.filter((id, index) => !memberSet.has(id) && exact.indexOf(id) === index),
  ];
}

function canonicalAgentToolset(
  previous: AgentManagedToolset | undefined,
  selected: Set<string>,
  members: readonly AgentToolsetMemberCap[],
  controlled: boolean,
): AgentManagedToolset {
  const previousByName = new Map(
    (previous?.configs ?? []).map((config) => [config.name, config]),
  );
  const memberNames = new Set(members.map((member) => member.name));
  const controlledMembers = new Set(controlledModificationMemberNames(members));
  const configs = members.flatMap((member): AgentToolsetConfig[] => {
    const name = member.name;
    const existing = previousByName.get(name);
    const permission = controlled && controlledMembers.has(name)
      ? ALWAYS_ASK
      : effectivePermission(previous, existing);
    const enabled = selected.has(name);
    const hasExecutionConfiguration = existing !== undefined
      && Object.keys(existing).some((key) => ![
        "name",
        "type",
        "enabled",
        "permission_policy",
      ].includes(key));
    if (!enabled && permission.type === "always_allow" && !hasExecutionConfiguration) return [];
    return [{
      ...existing,
      name,
      enabled,
      permission_policy: permission,
    }];
  });
  const runtimeOnly = (previous?.configs ?? []).filter(
    (config) => !memberNames.has(config.name),
  );
  return {
    type: "agent_toolset_20260401",
    default_config: { enabled: false, permission_policy: ALWAYS_ALLOW },
    configs: [...configs, ...runtimeOnly],
  };
}

function projectAgentTools(
  tools: readonly AgentTool[],
  selectedIds: readonly string[],
  members: readonly AgentToolsetMemberCap[],
  controlled: boolean,
): AgentTool[] {
  const memberSet = new Set(members.map((member) => member.name));
  const selected = new Set(selectedIds.filter((id) => memberSet.has(id)));
  const exact = selectedIds.filter(
    (id, index) => !memberSet.has(id) && selectedIds.indexOf(id) === index,
  );
  const previous = tools.find(isAgentToolset);
  const nonAgentObjects = tools.filter(
    (tool): tool is Exclude<AgentTool, string | AgentManagedToolset> =>
      typeof tool !== "string" && !isAgentToolset(tool),
  );
  const needsAgentToolset = controlled || selected.size > 0 || previous !== undefined;
  return [
    ...(needsAgentToolset
      ? [canonicalAgentToolset(previous, selected, members, controlled)]
      : []),
    ...exact,
    ...nonAgentObjects,
  ];
}

/** Replace picker selection through one typed Agent Toolset, preserving MCP/custom tools. */
export function withSelectedAgentTools(
  tools: readonly AgentTool[],
  selectedIds: readonly string[],
  members: readonly AgentToolsetMemberCap[],
): AgentTool[] {
  return projectAgentTools(tools, selectedIds, members, false);
}

/** Pure UI projection: no preset marker or parallel permission document is persisted. */
export function controlledModificationPatch(
  config: AgentConfig,
  members: readonly AgentToolsetMemberCap[],
): Partial<AgentConfig> {
  const pluginConfig = { ...config.plugin_config };
  delete pluginConfig.permission;
  return {
    tools: projectAgentTools(
      config.tools,
      selectedAgentToolIds(config.tools, members),
      members,
      true,
    ),
    plugins: config.plugins.filter((id) => id !== "permission"),
    plugin_config: pluginConfig,
  };
}
