import assert from 'node:assert/strict';
import Anthropic from '@anthropic-ai/sdk';
import type {
  BetaManagedAgentsMCPToolsetParams,
  BetaManagedAgentsURLMCPServerParams,
} from '@anthropic-ai/sdk/resources/beta/agents/agents';
import type { BetaManagedAgentsAgentWithOverridesParams } from '@anthropic-ai/sdk/resources/beta/sessions/sessions';

export type McpServer = BetaManagedAgentsURLMCPServerParams;

export function alwaysAllowMcpTools(
  servers: McpServer[],
): BetaManagedAgentsMCPToolsetParams[] {
  const serverNames = [...new Set(servers.map((server) => server.name))];
  return serverNames.map((mcpServerName) => ({
    type: 'mcp_toolset',
    mcp_server_name: mcpServerName,
    default_config: {
      enabled: true,
      permission_policy: { type: 'always_allow' },
    },
  }));
}

/**
 * Build the one official Session agent override used by protocol-focused MCP
 * tests. Managed Agents defaults MCP tools to `always_ask`; these scenarios
 * test transport, credential injection, replacement, or recovery rather than
 * HITL, so they must opt into execution explicitly.
 */
export function alwaysAllowMcpAgent(
  id: string,
  servers: McpServer[],
): BetaManagedAgentsAgentWithOverridesParams {
  return {
    id,
    type: 'agent_with_overrides',
    mcp_servers: servers,
    tools: alwaysAllowMcpTools(servers),
  };
}

export async function sendManagedMessage(
  client: Anthropic,
  sessionId: string,
  text: string,
  betas: string[],
): Promise<void> {
  await client.beta.sessions.events.send(sessionId, {
    events: [{ type: 'user.message', content: [{ type: 'text', text }] }],
    betas,
  });
}

export function replaceMcpServers(
  client: Anthropic,
  sessionId: string,
  servers: McpServer[],
  betas: string[],
  headers: Record<string, string>,
) {
  return client.beta.sessions
    .update(sessionId, { agent: { mcp_servers: servers }, betas }, { headers })
    .withResponse();
}

export function responseEtag(response: Response): string {
  const value = response.headers.get('etag');
  assert.ok(value, 'a successful Session mutation returns ETag');
  return value;
}

/** Read the one current Session root revision used by If-Match CAS commands. */
export async function retrieveSessionWithEtag(
  client: Anthropic,
  sessionId: string,
  betas: string[],
) {
  const retrieved = await client.beta.sessions.retrieve(sessionId, { betas }).withResponse();
  return { session: retrieved.data, etag: responseEtag(retrieved.response) };
}
