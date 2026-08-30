import type {
  AgentConfig,
  AgentMcpServer,
  AgentTool,
  AgentToolset,
  ManagedToolsetCap,
} from "./api/types";

export type ToolPermission = "always_allow" | "always_ask";

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

export function isAgentToolset(tool: AgentTool): tool is AgentToolset {
  return typeof tool === "object" && tool.type === "agent_toolset_20260401";
}

export function isMcpToolset(tool: AgentTool): tool is AgentToolset & { type: "mcp_toolset"; mcp_server_name: string } {
  return typeof tool === "object" && tool.type === "mcp_toolset" && typeof tool.mcp_server_name === "string";
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
    && candidate.type === "mcp_toolset"
    && candidate.mcp_server_name === serverName);
  const requestedPermission = toolset?.default_config?.permission_policy?.type;
  return {
    enabled: toolset?.default_config?.enabled !== false,
    permission: requestedPermission === "always_allow" ? "always_allow" : "always_ask",
    namedOverrides: toolset?.configs?.length ?? 0,
  };
}

export function mcpDefaultConfig(capabilities: ManagedToolsetCap[] | undefined): AgentToolset["default_config"] {
  const advertised = capabilities?.find((capability) => capability.type === "mcp_toolset")?.default_config;
  return advertised
    ? { enabled: advertised.enabled, permission_policy: { ...advertised.permission_policy } }
    : { enabled: true, permission_policy: { type: "always_ask" } };
}

export function reconcileMcpToolsets(
  tools: AgentTool[],
  servers: AgentMcpServer[],
  defaultConfig: AgentToolset["default_config"],
): AgentTool[] {
  const ordinary = tools.filter((tool) => !isMcpToolset(tool));
  const existing = new Map(
    tools.filter(isMcpToolset).map((toolset) => [toolset.mcp_server_name, toolset]),
  );
  const policies = servers.map((server): AgentToolset => existing.get(server.name) ?? {
    type: "mcp_toolset",
    mcp_server_name: server.name,
    configs: [],
    default_config: defaultConfig,
  });
  return [...ordinary, ...policies];
}

export function renameMcpToolset(tools: AgentTool[], previous: string, next: string): AgentTool[] {
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

export function removeMcpIntegration(config: AgentConfig, serverName: string): Partial<AgentConfig> {
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
