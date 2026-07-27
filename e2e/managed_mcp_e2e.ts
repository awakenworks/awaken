// Managed-agents MCP + vault e2e driven by the **official** Anthropic
// TypeScript SDK. A session binds an inline MCP server (`mcp_servers`) to the
// first matching vault-held `static_bearer` credential in `vault_ids`, using
// normalized `mcp_server_url` equality. The deterministic MCP model (`add <a> <b>` →
// `mcp__calc__add` → `result: <sum>`) converses across THREE turns on one
// session. The Node fixture mirrors the in-process Rust mock from
// crates/agents/awaken-server/tests/mcp_sessions.rs and records the
// Authorization header, so this proves the vault-materialized bearer actually
// crossed the wire to the MCP server throughout initialize → tools/list →
// tools/call. OAuth and refresh semantics remain in managed_mcp_refresh_e2e.mjs.
//
// Run: (from e2e/)  npm install && node managed_mcp_e2e.ts

import assert from 'node:assert/strict';
import Anthropic from '@anthropic-ai/sdk';
// The production harness and fixture remain JavaScript so they can also run in
// the repository's plain-Node matrix; this test gives the SDK boundary its own
// compile-time coverage and types the values consumed below.
// @ts-ignore -- declarations are intentionally local to the JS harness matrix.
import { withScenarioServer, pass } from './harness.mjs';
// @ts-ignore -- declarations are intentionally local to the JS harness matrix.
import { startCalcFixture } from './fixtures/mcp_calc_fixture.mjs';
import { alwaysAllowMcpAgent, sendManagedMessage } from './fixtures/managed_mcp_session.ts';

const BETAS = ['managed-agents-2026-04-01'];
const CALC_TOKEN = 'calc-bearer-token-e2e'; // awaken-allow: secret
const LOSING_TOKEN = 'calc-losing-token-e2e'; // awaken-allow: secret

type SessionEvent = { type: string; [key: string]: any };

async function listEvents(client: Anthropic, sessionId: string): Promise<SessionEvent[]> {
  const events: SessionEvent[] = [];
  for await (const ev of client.beta.sessions.events.list(sessionId, { betas: BETAS })) events.push(ev);
  return events;
}

const textFromContent = (content: unknown): string => {
  const block = Array.isArray(content) ? content[0] : undefined;
  return block && typeof block === 'object' && 'text' in block && typeof block.text === 'string'
    ? block.text
    : '';
};

const agentMessages = (events: SessionEvent[]): string[] =>
  events.filter((e) => e.type === 'agent.message').map((e) => textFromContent(e.content));

async function main(): Promise<void> {
  const fixture: {
    url: string;
    calls: Array<{ method: string; authorization: string }>;
    unauthorized: number;
    close(): Promise<void>;
  } = await startCalcFixture(CALC_TOKEN);
  try {
    await withScenarioServer('management', 'mcp', 38190, async (baseUrl: string) => {
      const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: baseUrl });

      // Create the losing credential first so the former global minimum-id
      // implementation would choose it and fail. The caller's vault_ids order,
      // not creation/id order, is the authoritative precedence contract.
      const losingVault = await client.beta.vaults.create({
        display_name: 'MCP losing vault',
        betas: BETAS,
      });
      const normalizedWithoutSlash = fixture.url.slice(0, -1);
      const losingCredential = await client.beta.vaults.credentials.create(losingVault.id, {
        auth: {
          type: 'static_bearer',
          mcp_server_url: normalizedWithoutSlash,
          token: LOSING_TOKEN,
        },
        betas: BETAS,
      });
      assert.equal(losingCredential.auth.type, 'static_bearer');
      assert.ok(!JSON.stringify(losingCredential).includes(LOSING_TOKEN), 'losing token must not be echoed');

      const preferredVault = await client.beta.vaults.create({
        display_name: 'MCP preferred vault',
        betas: BETAS,
      });
      const preferredCredential = await client.beta.vaults.credentials.create(preferredVault.id, {
        auth: {
          type: 'static_bearer',
          mcp_server_url: normalizedWithoutSlash,
          token: CALC_TOKEN,
        },
        betas: BETAS,
      });
      assert.equal(preferredCredential.type, 'vault_credential');
      assert.equal(preferredCredential.auth.type, 'static_bearer');
      assert.equal(
        preferredCredential.auth.mcp_server_url,
        normalizedWithoutSlash,
        'the authored URL is echoed without rewriting the public resource',
      );
      assert.ok(!JSON.stringify(preferredCredential).includes(CALC_TOKEN), 'preferred token must not be echoed');
      pass('official SDK creates two secret-free static_bearer credentials for one normalized MCP URL');

      // The Session URL includes the trailing slash while both credential URLs
      // omit it. The preferred vault is deliberately second-created but first
      // in vault_ids; a successful handshake therefore proves both contracts.
      const calcServer = { name: 'calc', type: 'url' as const, url: fixture.url };
      const session = await client.beta.sessions.create({
        agent: alwaysAllowMcpAgent('assistant', [calcServer]),
        environment_id: 'env_local',
        vault_ids: [preferredVault.id, losingVault.id],
        betas: BETAS,
      });
      assert.deepEqual(
        session.agent.mcp_servers,
        [calcServer],
        'session.agent.mcp_servers echoes the binding',
      );
      pass('Session binds the normalized URL using caller-supplied vault_ids precedence');

      // --- turn 1: the agent calls the MCP tool and reports the sum ---
      await sendManagedMessage(client, session.id, 'add 2 3', BETAS);
      let events = await listEvents(client, session.id);
      // An MCP tool call projects as the distinct agent.mcp_tool_use/result events.
      const toolUse = events.find((e) => e.type === 'agent.mcp_tool_use');
      assert.ok(toolUse, `an agent.mcp_tool_use event: ${JSON.stringify(events.map((e) => e.type))}`);
      assert.equal(toolUse.name, 'mcp__calc__add');
      assert.equal(toolUse.mcp_server_name, 'calc');
      const toolResult = events.find((e) => e.type === 'agent.mcp_tool_result');
      assert.ok(
        toolResult,
        `an agent.mcp_tool_result event: ${JSON.stringify(events)}`,
      );
      assert.equal(toolResult.mcp_tool_use_id, toolUse.id);
      assert.equal(textFromContent(toolResult.content), '5');
      assert.ok(
        agentMessages(events).some((m) => m.includes('result: 5')),
        `final message reports result: 5 — got ${JSON.stringify(agentMessages(events))}`,
      );
      pass('turn 1: add 2 3 -> mcp__calc__add mcp_tool_use, mcp_tool_result 5, "result: 5"');

      // --- turn 2 on the SAME session: the connection serves the next turn ---
      await sendManagedMessage(client, session.id, 'add 40 2', BETAS);
      events = await listEvents(client, session.id);
      assert.ok(
        events.some((e) => e.type === 'agent.mcp_tool_result' && textFromContent(e.content) === '42'),
        'second turn mcp tool result is 42',
      );
      assert.ok(
        agentMessages(events).some((m) => m.includes('result: 42')),
        'second turn final message reports result: 42',
      );
      pass('turn 2 (same session): add 40 2 -> tool_result 42, "result: 42"');

      // --- turn 3: a plain message echoes (no tool call) ---
      await sendManagedMessage(client, session.id, 'just chatting', BETAS);
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
        fixture.calls.some((c) => c.method === 'initialize') &&
          fixture.calls.some((c) => c.method === 'tools/list'),
        `complete MCP handshake and discovery ran: ${JSON.stringify(fixture.calls.map((c) => c.method))}`,
      );
      assert.ok(
        fixture.calls.every((c) => c.authorization === `Bearer ${CALC_TOKEN}`),
        'every JSON-RPC request carried the bearer',
      );
      assert.ok(
        fixture.calls.every((c) => c.authorization !== `Bearer ${LOSING_TOKEN}`),
        'the lower-id credential from the lower-priority vault was never injected',
      );
      assert.equal(fixture.unauthorized, 0, 'no request was ever rejected for missing auth');
      pass('initialize, tools/list and both tools/call requests carried only the preferred bearer');
    });

    console.log('E2E PASS: TypeScript SDK vault injection preserves the complete multi-turn MCP protocol.');
    process.exitCode = 0;
  } catch (err) {
    console.error('E2E FAIL:', err);
    process.exitCode = 1;
  } finally {
    await fixture.close();
  }
}

main();
