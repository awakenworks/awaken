import assert from 'node:assert/strict';
import Anthropic from '@anthropic-ai/sdk';
import type {
  BetaManagedAgentsMCPToolsetParams,
  BetaManagedAgentsURLMCPServerParams,
} from '@anthropic-ai/sdk/resources/beta/agents/agents';
import type { BetaManagedAgentsSessionEvent } from '@anthropic-ai/sdk/resources/beta/sessions/events';
import type { BetaManagedAgentsAgentWithOverridesParams } from '@anthropic-ai/sdk/resources/beta/sessions/sessions';
// @ts-ignore -- the shared JavaScript harness deliberately serves TypeScript fixtures.
import { retryableReadStatus, waitForSessionEventReceipt } from '../harness.mjs';

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
): Promise<BetaManagedAgentsSessionEvent[]> {
  // Cause/effect graph: C1=the official SDK returns the exact durable User
  // Event receipt; C2=the Session lifecycle may process its Run later; C3=the
  // idempotent history read may report a classified transient after recovery;
  // C4=history reports an unclassified error or C3 persists to the deadline.
  // Effects: E1=observe that receipt with processed_at; E2=observe its later
  // committed idle terminal before any MCP fixture/event assertion; E3=retry
  // only events.list after C3; E4=surface C4 exactly. Constraint: the unkeyed
  // Event send runs once and is never replayed here; listing committed events is
  // observation only, and this helper may not drive the Runtime or accept an
  // older Run's terminal. Decision rules: W1 !E1=>retry read; W2 E1&&!E2=>retry
  // read; W3 E1&&E2=>return; W4 C3=>E3; W5 C4=>E4.
  const receipt = await client.beta.sessions.events.send(sessionId, {
    events: [{ type: 'user.message', content: [{ type: 'text', text }] }],
    betas,
  });
  const acceptedId = receipt.data?.[0]?.id;
  assert.equal(typeof acceptedId, 'string', 'Managed send returns the exact User Event receipt');
  const { events } = await waitForSessionEventReceipt(
    client,
    sessionId,
    acceptedId,
    betas,
    ({ delta }: { delta: BetaManagedAgentsSessionEvent[] }) => delta
      .some((event) => event.type === 'session.status_idle'),
    `Managed Run for ${JSON.stringify(text)} to commit after its exact receipt`,
    {
      // A seqlock conflict means this one list attempt could not assemble a
      // stable prefix. Keep the accepted receipt identity and repeat only GET.
      retryReadError: (error: unknown) => retryableReadStatus(error) !== null,
    },
  );
  return events;
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
