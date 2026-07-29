// Managed-API ACP × MCP credential-boundary e2e. It proves two adjacent rules:
// an authenticated MCP fails closed without provider no-bypass evidence, while an
// anonymous MCP crosses the REAL ACP `session/new` codec as a route with no auth field.
// A credential reference or plaintext bearer is never an ACP compatibility fallback.
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
  // MCP client; the vault-materialized bearer remains in the host-owned relay.
  const fixture = await startCalcFixture(CALC_TOKEN, { allowAnonymous: true });
  const replacementFixture = await startCalcFixture(undefined, { allowAnonymous: true });
  try {
    await withServer('acp-managed-mcp', 38196, async (baseUrl) => {
      const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: baseUrl });
      const acpAgent = await client.beta.agents.create({
        name: 'managed ACP fixture',
        model: 'acp-managed-mcp',
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
      // C1 MCP has a credential -> C2 selected holder is Worker
      // C2 + C3 provider proves no-bypass -> E1 realize relay
      // C2 + !C3 -> E2 reject before materialization
      // !C1 -> E3 project direct route with no ACP auth
      // C4 idle Session replaces active generation -> E4 next ACP launch receives only replacement
      // C5 idle Session removes active generation -> E5 next ACP launch receives an empty MCP set
      //
      // | Rule | desired set | credential | no-bypass | Result |
      // | A1 | calc | yes | no | reject |
      // | A2 | calc | no | - | initial session/new contains calc |
      // | A3 | search replaces calc | no | - | relaunched ACP contains search, not calc |
      // | A4 | empty replaces search | no | - | relaunched ACP contains neither server |
      // A provider-backed success rule is covered by the provider conformance slice;
      // this composition intentionally installs the Workdir provider.
      await assert.rejects(
        client.beta.sessions.create({
          agent: acpAgent.id,
          mcp_servers: [{ name: 'calc', type: 'url', url: fixture.url }],
          vault_ids: [vault.id],
          betas: BETAS,
        }),
        (error) => error?.status === 500 &&
          error?.message?.includes('provider-enforced secret substitution and no-bypass networking'),
        'A1: authenticated ACP MCP must fail closed without provider evidence',
      );

      const anonymous = await client.beta.sessions.create({
        agent: acpAgent.id,
        mcp_servers: [{ name: 'calc', type: 'url', url: fixture.url }],
        betas: BETAS,
      });
      await client.beta.sessions.events.send(anonymous.id, {
        events: [{ type: 'user.message', content: [{ type: 'text', text: 'hello' }] }],
        betas: BETAS,
      });

      const texts = await agentTexts(client, anonymous.id);
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
      await client.beta.sessions.events.send(anonymous.id, {
        events: [{ type: 'user.message', content: [{ type: 'text', text: 'after replace' }] }],
        betas: BETAS,
      });
      const afterReplace = await agentTexts(client, anonymous.id);
      assert.equal(afterReplace.at(-1), 'mcp saw-search noref', `A3: ${JSON.stringify(afterReplace)}`);
      assert.ok(!afterReplace.at(-1).includes('saw-calc'), 'A3 old generation is absent');

      const removed = await client.beta.sessions.update(anonymous.id, {
        agent: { mcp_servers: [] },
        betas: BETAS,
      });
      assert.deepEqual(removed.agent.mcp_servers, [], 'A4 projects the drained set');
      await client.beta.sessions.events.send(anonymous.id, {
        events: [{ type: 'user.message', content: [{ type: 'text', text: 'after remove' }] }],
        betas: BETAS,
      });
      const afterRemove = await agentTexts(client, anonymous.id);
      assert.equal(afterRemove.at(-1), 'mcp noname noref', `A4: ${JSON.stringify(afterRemove)}`);
      pass('A1-A4 ACP MCP create, replace, and remove consume only the active generation');
    });

    // Compose the real-CLI twin through the same external API without launching the
    // networked CLI. This keeps the real factory in the hermetic coverage gate and
    // guards against reintroducing the obsolete trusted-inline override; the dynamic
    // host-owned relay behavior itself is exercised above, while the live twin is exercised by
    // acp_real_mcp_kimi_e2e.mjs when provider quota is available.
    await withServer('acp-real-mcp', 38199, async (baseUrl) => {
      const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: baseUrl });
      const acpAgent = await client.beta.agents.create({
        name: 'real ACP fixture',
        model: process.env.ANTHROPIC_MODEL || 'acp-real-mcp',
        betas: BETAS,
      });
      const session = await client.beta.sessions.create({
        agent: acpAgent.id,
        betas: BETAS,
      });
      assert.equal(session.status, 'idle');
      assert.equal(session.agent.id, acpAgent.id);
      pass('real ACP factory composes behind the managed API without a trusted-inline MCP path');
    });

    console.log('E2E PASS: managed ACP × MCP credential boundary and real-CLI factory wiring.');
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
