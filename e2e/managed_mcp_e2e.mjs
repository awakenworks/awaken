// Managed-agents MCP + vault e2e driven by the **official** Anthropic
// TypeScript SDK. A session binds an inline MCP server (`mcp_servers`) to a
// vault-held `mcp_oauth` credential (`vault_ids`, matched by exact
// `mcp_server_url`), and the deterministic MCP model (`add <a> <b>` →
// `mcp__calc__add` → `result: <sum>`) converses across THREE turns on one
// session. The Node fixture mirrors the in-process Rust mock from
// crates/agents/awaken-server-local/tests/mcp_sessions.rs and records the
// Authorization header, so this proves the vault-materialized bearer actually
// crossed the wire to the MCP server.
//
// Run: (from e2e/)  npm install && node managed_mcp_e2e.mjs

import assert from 'node:assert/strict';
import Anthropic from '@anthropic-ai/sdk';
import { withScenarioServer, pass } from './harness.mjs';
import { startCalcFixture } from './fixtures/mcp_calc_fixture.mjs';

const BETAS = ['managed-agents-2026-04-01'];
const CALC_TOKEN = 'calc-bearer-token-e2e'; // awaken-allow: secret

async function listEvents(client, sessionId) {
  const events = [];
  for await (const ev of client.beta.sessions.events.list(sessionId, { betas: BETAS })) events.push(ev);
  return events;
}

async function sendMessage(client, sessionId, text) {
  await client.beta.sessions.events.send(sessionId, {
    events: [{ type: 'user.message', content: [{ type: 'text', text }] }],
    betas: BETAS,
  });
}

const agentMessages = (events) =>
  events.filter((e) => e.type === 'agent.message').map((e) => e.content[0].text);

async function main() {
  const fixture = await startCalcFixture(CALC_TOKEN);
  try {
    await withScenarioServer('management', 'mcp', 38190, async (baseUrl) => {
      const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: baseUrl });

      // --- vault + mcp_oauth credential (secret write-only) ---
      const vault = await client.beta.vaults.create({ display_name: 'MCP vault', betas: BETAS });
      assert.equal(vault.type, 'vault');
      const cred = await client.beta.vaults.credentials.create(vault.id, {
        type: 'mcp_oauth',
        mcp_server_url: fixture.url,
        access_token: CALC_TOKEN,
        betas: BETAS,
      });
      assert.equal(cred.type, 'vault_credential');
      assert.equal(cred.auth.type, 'mcp_oauth');
      assert.equal(cred.auth.mcp_server_url, fixture.url, 'mcp_server_url echoed');
      assert.ok(!JSON.stringify(cred).includes(CALC_TOKEN), 'access token must not be echoed');
      pass('beta.vaults.credentials.create(mcp_oauth) -> secret-free, mcp_server_url echoed');

      // --- session with an inline MCP server bound to the vault ---
      const session = await client.beta.sessions.create({
        agent: 'assistant',
        mcp_servers: [{ name: 'calc', type: 'url', url: fixture.url }],
        vault_ids: [vault.id],
        betas: BETAS,
      });
      assert.deepEqual(
        session.agent.mcp_servers,
        [{ name: 'calc', type: 'url', url: fixture.url }],
        'session.agent.mcp_servers echoes the binding',
      );
      pass('beta.sessions.create with mcp_servers + vault_ids echoes agent.mcp_servers');

      // --- turn 1: the agent calls the MCP tool and reports the sum ---
      await sendMessage(client, session.id, 'add 2 3');
      let events = await listEvents(client, session.id);
      const toolUse = events.find((e) => e.type === 'agent.tool_use');
      assert.ok(toolUse, `an agent.tool_use event: ${JSON.stringify(events.map((e) => e.type))}`);
      assert.equal(toolUse.name, 'mcp__calc__add');
      const toolResult = events.find((e) => e.type === 'agent.tool_result');
      assert.ok(toolResult, 'an agent.tool_result event');
      assert.equal(toolResult.content[0].text, '5');
      assert.ok(
        agentMessages(events).some((m) => m.includes('result: 5')),
        `final message reports result: 5 — got ${JSON.stringify(agentMessages(events))}`,
      );
      pass('turn 1: add 2 3 -> mcp__calc__add tool_use, tool_result 5, "result: 5"');

      // --- turn 2 on the SAME session: the connection serves the next turn ---
      await sendMessage(client, session.id, 'add 40 2');
      events = await listEvents(client, session.id);
      assert.ok(
        events.some((e) => e.type === 'agent.tool_result' && e.content[0].text === '42'),
        'second turn tool result is 42',
      );
      assert.ok(
        agentMessages(events).some((m) => m.includes('result: 42')),
        'second turn final message reports result: 42',
      );
      pass('turn 2 (same session): add 40 2 -> tool_result 42, "result: 42"');

      // --- turn 3: a plain message echoes (no tool call) ---
      await sendMessage(client, session.id, 'just chatting');
      events = await listEvents(client, session.id);
      assert.ok(
        agentMessages(events).includes('Echo: just chatting'),
        `plain text echoes — got ${JSON.stringify(agentMessages(events))}`,
      );
      pass('turn 3 (same session): plain message -> Echo reply');

      // --- the fixture saw the vault-materialized bearer on tools/call ---
      const toolCalls = fixture.calls.filter((c) => c.method === 'tools/call');
      assert.equal(toolCalls.length, 2, `two tools/call requests, got ${toolCalls.length}`);
      for (const c of toolCalls) assert.equal(c.authorization, `Bearer ${CALC_TOKEN}`);
      assert.ok(
        fixture.calls.every((c) => c.authorization === `Bearer ${CALC_TOKEN}`),
        'every JSON-RPC request carried the bearer',
      );
      assert.equal(fixture.unauthorized, 0, 'no request was ever rejected for missing auth');
      pass('fixture saw `Bearer <token>` on every request incl. both tools/call');
    });

    console.log('E2E PASS: managed MCP + vault multi-turn round-trips through the official @anthropic-ai/sdk.');
    process.exitCode = 0;
  } catch (err) {
    console.error('E2E FAIL:', err);
    process.exitCode = 1;
  } finally {
    await fixture.close();
  }
}

main();
