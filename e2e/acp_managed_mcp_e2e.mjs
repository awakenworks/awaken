// Managed-API ACP × MCP credential-boundary e2e. It proves two adjacent rules:
// an authenticated MCP reaches a catalog-declared ACP adapter through its
// process-private `session/new` header, while an anonymous MCP crosses the same
// codec as a route with no auth field. No credential is echoed into public events.
//
// Run: (from e2e/)  node acp_managed_mcp_e2e.mjs

import assert from 'node:assert/strict';
import Anthropic from '@anthropic-ai/sdk';
import { withServer, pass, waitForSessionEventReceipt } from './harness.mjs';
import { startCalcFixture } from './fixtures/mcp_calc_fixture.mjs';

const BETAS = ['managed-agents-2026-04-01'];
const CALC_TOKEN = 'calc-bearer-token-e2e'; // awaken-allow: secret

function agentTexts(events) {
  return events
    .filter((event) => event.type === 'agent.message')
    .map((event) => (event.content ?? []).map((content) => content.text ?? '').join(''));
}

async function runAcpTurn(client, sessionId, text, rule) {
  // C1=exact ACP User receipt; C2=active MCP-generation reply+terminal.
  // E1=C2 after C1 proves the selected generation. K: each update replaces the
  // Session root; no prior reply is eligible. Decision A1 C1&&!C2=>retry;
  // A2 C1+C2=>return receipt-scoped history.
  const receipt = await client.beta.sessions.events.send(sessionId, {
    events: [{ type: 'user.message', content: [{ type: 'text', text }] }],
    betas: BETAS,
  });
  const receiptId = receipt.data[0]?.id;
  assert.equal(typeof receiptId, 'string', `${rule} exact ACP MCP User Event receipt`);
  const { events } = await waitForSessionEventReceipt(
    client,
    sessionId,
    receiptId,
    BETAS,
    ({ delta }) => delta.some((event) => event.type === 'agent.message')
      && delta.some((event) => event.type === 'session.status_idle'),
    `${rule} ACP MCP Run to commit`,
  );
  return agentTexts(events);
}

async function main() {
  // The host connects to this MCP server in-process at prepare (tool discovery +
  // pre-authorization), so it must be reachable even though the ACP CLI has its own
  // MCP client; the vault-materialized bearer remains in the host-owned relay.
  const fixture = await startCalcFixture(CALC_TOKEN, { allowAnonymous: true });
  const replacementFixture = await startCalcFixture(undefined, { allowAnonymous: true });
  try {
    await withServer('acp-managed-mcp', 38196, async (baseUrl) => {
      const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: baseUrl });
      const acpAgent = await client.beta.agents.create({
        name: 'managed ACP fixture',
        model: 'acp-managed-mcp',
        mcp_servers: [{ name: 'calc', type: 'url', url: fixture.url }],
        tools: [{ type: 'mcp_toolset', mcp_server_name: 'calc' }],
        betas: BETAS,
      });

      // A vault + mcp_oauth credential holding the MCP server's token (write-only).
      const vault = await client.beta.vaults.create({ display_name: 'ACP MCP vault', betas: BETAS });
      const cred = await client.beta.vaults.credentials.create(vault.id, {
        type: 'mcp_oauth',
        mcp_server_url: fixture.url,
        access_token: CALC_TOKEN,
        betas: BETAS,
      });
      assert.ok(!JSON.stringify(cred).includes(CALC_TOKEN), 'the access token is never echoed');

      // Cause-effect graph:
      // C1 MCP has a credential -> C2 catalog declares client injection
      // C1 + C2 -> E1 process-private Session auth
      // C1 + !C2 -> E2 reject before materialization (unit/conformance slice)
      // !C1 -> E3 project direct route with no ACP auth
      // C4 idle Session replaces active generation -> E4 next ACP launch receives only replacement
      // C5 idle Session removes active generation -> E5 next ACP launch receives an empty MCP set
      //
      // | Rule | desired set | credential | adapter declaration | Result |
      // | A1 | calc | yes | yes | process-private auth |
      // | A2 | calc | no | - | initial session/new contains calc |
      // | A3 | search replaces calc | no | - | relaunched ACP contains search, not calc |
      // | A4 | empty replaces search | no | - | relaunched ACP contains neither server |
      // Unknown/custom-adapter rejection is covered by the adapter declaration
      // conformance slice; this composition exercises the declared Claude path.
      const authenticated = await client.beta.sessions.create({
        agent: acpAgent.id,
        environment_id: 'env_local',
        vault_ids: [vault.id],
        betas: BETAS,
      });
      const authenticatedTexts = await runAcpTurn(
        client,
        authenticated.id,
        'use calc privately',
        'A1',
      );
      assert.ok(
        authenticatedTexts.includes('mcp saw-calc process-auth'),
        `A1: declared adapter receives process-private auth: ${JSON.stringify(authenticatedTexts)}`,
      );
      assert.ok(
        !JSON.stringify(authenticatedTexts).includes(CALC_TOKEN),
        'A1: public agent output contains no credential material',
      );

      const anonymous = await client.beta.sessions.create({
        agent: acpAgent.id,
        environment_id: 'env_local',
        betas: BETAS,
      });
      const texts = await runAcpTurn(client, anonymous.id, 'hello', 'A2');
      const reply = texts.join(' ');
      assert.ok(
        texts.includes('mcp saw-calc noref'),
        `A2: session/new carried the anonymous route without auth, got ${JSON.stringify(texts)}`,
      );
      assert.ok(!reply.includes(CALC_TOKEN), 'the raw vault token never reached the CLI');
      const search = { name: 'search', type: 'url', url: replacementFixture.url };
      const replaced = await client.beta.sessions.update(anonymous.id, {
        agent: { mcp_servers: [search] },
        betas: BETAS,
      });
      assert.deepEqual(replaced.agent.mcp_servers, [search], 'A3 projects only replacement');
      const afterReplace = await runAcpTurn(client, anonymous.id, 'after replace', 'A3');
      assert.equal(afterReplace.at(-1), 'mcp saw-search noref', `A3: ${JSON.stringify(afterReplace)}`);
      assert.ok(!afterReplace.at(-1).includes('saw-calc'), 'A3 old generation is absent');

      const removed = await client.beta.sessions.update(anonymous.id, {
        agent: { mcp_servers: [] },
        betas: BETAS,
      });
      assert.deepEqual(removed.agent.mcp_servers, [], 'A4 projects the drained set');
      const afterRemove = await runAcpTurn(client, anonymous.id, 'after remove', 'A4');
      assert.equal(afterRemove.at(-1), 'mcp noname noref', `A4: ${JSON.stringify(afterRemove)}`);
      pass('A1-A4 ACP MCP create, replace, and remove consume only the active generation');
    });

    // Coverage ownership: this hermetic scenario owns only A1-A4. The live
    // `acp_real_mcp_kimi_e2e` owns pinned-wrapper launch plus real model/MCP
    // effects, while the executor's retained-inline test owns legacy credential
    // rejection. A second idle-Session composition has no distinct effect and
    // must not turn this deterministic gate into an npm/network acquisition test.

    console.log('E2E PASS: managed ACP × MCP credential boundary.');
    process.exitCode = 0;
  } catch (err) {
    console.error('E2E FAIL:', err);
    process.exitCode = 1;
  } finally {
    await fixture.close();
    await replacementFixture.close();
  }
}

main();
