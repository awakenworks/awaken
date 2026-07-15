// Managed-API ACP × MCP × vault e2e: a session that selects an external ACP CLI
// runtime (`awaken.runtime: acp:*`) AND declares an inline `mcp_servers` bound to a
// vault credential has its staged MCP server projected — α-secretless — into the CLI's
// `session/new` request. Proves the whole D6→D5 chain end to end through the HTTP
// managed API: session mcp_servers → prepare_session staging (vault-materialized bearer)
// → α overlay (`session-mcp:<name>` reference, never the raw token) → `plugin_config.acp`
// → the ProjectingChannelSource's `session/new` injection over the REAL ACP JSON-RPC
// codec. The fake CLI echoes what it saw on `session/new`, so the assertion is on the
// bearer form that actually crossed to the agent. `AWAKEN_MODEL_MODE=acp-managed-mcp`.
//
// Run: (from e2e/)  node acp_managed_mcp_e2e.mjs

import assert from 'node:assert/strict';
import Anthropic from '@anthropic-ai/sdk';
import { withServer, pass } from './harness.mjs';
import { startCalcFixture } from './fixtures/mcp_calc_fixture.mjs';

const BETAS = ['managed-agents-2026-04-01'];
const CALC_TOKEN = 'calc-bearer-token-e2e'; // awaken-allow: secret

async function agentTexts(client, sessionId) {
  const texts = [];
  for await (const ev of client.beta.sessions.events.list(sessionId, { betas: BETAS })) {
    if (ev.type === 'agent.message') texts.push((ev.content ?? []).map((c) => c.text ?? '').join(''));
  }
  return texts;
}

async function main() {
  // The host connects to this MCP server in-process at prepare (tool discovery +
  // pre-authorization), so it must be reachable even though the ACP CLI has its own
  // MCP client; the vault-materialized bearer is what the α overlay then references.
  const fixture = await startCalcFixture(CALC_TOKEN);
  try {
    await withServer('acp-managed-mcp', 38196, async (baseUrl) => {
      const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: baseUrl });

      // A vault + mcp_oauth credential holding the MCP server's token (write-only).
      const vault = await client.beta.vaults.create({ display_name: 'ACP MCP vault', betas: BETAS });
      const cred = await client.beta.vaults.credentials.create(vault.id, {
        type: 'mcp_oauth',
        mcp_server_url: fixture.url,
        access_token: CALC_TOKEN,
        betas: BETAS,
      });
      assert.ok(!JSON.stringify(cred).includes(CALC_TOKEN), 'the access token is never echoed');

      // A session that BOTH selects the ACP CLI runtime and binds the MCP server.
      const session = await client.beta.sessions.create({
        agent: 'assistant',
        metadata: { 'awaken.runtime': 'acp:claude' },
        mcp_servers: [{ name: 'calc', type: 'url', url: fixture.url }],
        vault_ids: [vault.id],
        betas: BETAS,
      });
      await client.beta.sessions.events.send(session.id, {
        events: [{ type: 'user.message', content: [{ type: 'text', text: 'hello' }] }],
        betas: BETAS,
      });

      const texts = await agentTexts(client, session.id);
      const reply = texts.join(' ');
      // The fake CLI reports what crossed on `session/new`: the server name reached it,
      // and the bearer is the α reference (`session-mcp:calc`), not the raw vault token.
      assert.ok(
        texts.includes('mcp saw-calc alpha-ref'),
        `session/new carried the MCP server with the α reference to the ACP CLI, got ${JSON.stringify(texts)}`,
      );
      assert.ok(!reply.includes(CALC_TOKEN), 'the raw vault token never reached the CLI');
      pass('managed session (runtime acp:* + vault-bound mcp_servers) → α session-mcp reference on session/new, secretless');
    });

    console.log('E2E PASS: managed ACP × MCP α-secretless session/new injection end-to-end.');
    process.exitCode = 0;
  } catch (err) {
    console.error('E2E FAIL:', err);
    process.exitCode = 1;
  } finally {
    await fixture.close();
  }
}

main();
