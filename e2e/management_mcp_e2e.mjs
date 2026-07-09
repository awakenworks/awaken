// Management-plane MCP e2e: the whole agent↔MCP binding is authored through the
// ADMIN config surface (`/v1/config/*`, validated against the generated TS API
// contract with Ajv, like management_admin_e2e.mjs) — credential (secret-in) →
// McpServerDef (Exact binding) → AgentMcpConfig for `calc-agent` — then a
// managed session for that agent with NO inline mcp_servers converses
// multi-turn through the official Anthropic TypeScript SDK: the management
// plane's config supplies the server + credential. The Node fixture records
// the Authorization header, proving the admin-authored secret was materialized
// into the bearer the MCP server received.
//
// Run: (from e2e/)  npm install && node management_mcp_e2e.mjs

import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { Ajv2020 } from 'ajv/dist/2020.js';
import Anthropic from '@anthropic-ai/sdk';
import { withScenarioServer, pass } from './harness.mjs';
import { startCalcFixture } from './fixtures/mcp_calc_fixture.mjs';

const REPO_ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const CONTRACT = JSON.parse(
  fs.readFileSync(path.join(REPO_ROOT, 'contracts', 'model-schemas.json'), 'utf8'),
);
const ajv = new Ajv2020({ strict: false, allErrors: true });
const validators = Object.fromEntries(
  Object.entries(CONTRACT.schemas).map(([name, schema]) => [name, ajv.compile(schema)]),
);

const BETAS = ['managed-agents-2026-04-01'];
const CALC_TOKEN = 'calc-mgmt-bearer-token'; // awaken-allow: secret

/// Assert a value matches the generated schema `name`; throw with the Ajv errors.
function checkContract(name, value) {
  const validate = validators[name];
  assert.ok(validate, `no generated schema for ${name}`);
  const ok = validate(value);
  assert.ok(ok, `${name} response violates the generated contract: ${ajv.errorsText(validate.errors)}`);
}

async function req(base, method, uri, body) {
  const res = await fetch(`${base}${uri}`, {
    method,
    headers: body === undefined ? {} : { 'content-type': 'application/json' },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  const text = await res.text();
  const json = text ? JSON.parse(text) : null;
  return { status: res.status, json };
}

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

/// Assert one add-turn: tool_use mcp__calc__add + tool_result <sum> + "result: <sum>".
function assertAddTurn(events, sum) {
  // An MCP tool call projects as the distinct agent.mcp_tool_use / mcp_tool_result
  // events (not the builtin agent.tool_use), matching managed_mcp_e2e.
  assert.ok(
    events.some((e) => e.type === 'agent.mcp_tool_use' && e.name === 'mcp__calc__add'),
    `an mcp__calc__add mcp_tool_use: ${JSON.stringify(events.map((e) => e.type))}`,
  );
  assert.ok(
    events.some(
      (e) => e.type === 'agent.mcp_tool_result' && e.content[0].text === String(sum),
    ),
    `an mcp_tool_result of ${sum}`,
  );
  assert.ok(
    agentMessages(events).some((m) => m.includes(`result: ${sum}`)),
    `a final message reporting result: ${sum} — got ${JSON.stringify(agentMessages(events))}`,
  );
}

async function main() {
  const fixture = await startCalcFixture(CALC_TOKEN);
  try {
    await withScenarioServer('management', 'mcp', 38191, async (base) => {
      // --- author the binding through the admin config plane ---
      let r = await req(base, 'POST', '/v1/config/credentials', {
        workspace_id: 'ws',
        kind: 'vault',
        secret: CALC_TOKEN,
      });
      assert.equal(r.status, 201, JSON.stringify(r.json));
      checkContract('CredentialSource', r.json);
      assert.ok(!JSON.stringify(r.json).includes(CALC_TOKEN), 'secret must not be echoed');
      const credId = r.json.id;
      pass('POST /v1/config/credentials — secret-free CredentialSource matches contract');

      r = await req(base, 'PUT', '/v1/config/mcp-servers/calc-def', {
        id: 'calc-def',
        display_name: 'calc',
        url: fixture.url,
        credential_binding: { type: 'exact', credential_source_id: credId },
        version: 1,
      });
      assert.equal(r.status, 200, JSON.stringify(r.json));
      checkContract('McpServerDef', r.json);
      assert.equal(r.json.url, fixture.url);
      pass('PUT /v1/config/mcp-servers/calc-def — McpServerDef matches contract');

      r = await req(base, 'PUT', '/v1/config/agents/calc-agent/mcp', {
        agent_id: 'calc-agent',
        mcp_server_ids: ['calc-def'],
        version: 1,
      });
      assert.equal(r.status, 200, JSON.stringify(r.json));
      checkContract('AgentMcpConfig', r.json);
      assert.deepEqual(r.json.mcp_server_ids, ['calc-def']);
      pass('PUT /v1/config/agents/calc-agent/mcp — AgentMcpConfig matches contract');

      // --- dry-run the binding through the resolver: secret-free view ---
      r = await req(base, 'POST', '/v1/config/agents/calc-agent/mcp/resolve', {
        workspace_id: 'ws',
      });
      assert.equal(r.status, 200, JSON.stringify(r.json));
      assert.ok(Array.isArray(r.json) && r.json.length === 1, 'one resolved server');
      checkContract('ResolvedMcpServerView', r.json[0]);
      assert.equal(r.json[0].url, fixture.url);
      assert.equal(r.json[0].credential_present, true);
      assert.ok(!JSON.stringify(r.json).includes(CALC_TOKEN), 'resolve view is secret-free');
      pass('POST .../mcp/resolve — credential_present=true, secret absent');

      // --- a session for calc-agent with NO inline mcp_servers ---
      const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: base });
      const session = await client.beta.sessions.create({ agent: 'calc-agent', betas: BETAS });
      assert.equal(session.type, 'session');
      assert.equal(session.agent.id, 'calc-agent');

      await sendMessage(client, session.id, 'add 7 8');
      let events = await listEvents(client, session.id);
      assertAddTurn(events, 15);
      pass('turn 1: add 7 8 -> mcp__calc__add via the admin-authored config, result 15');

      await sendMessage(client, session.id, 'add 1 2');
      events = await listEvents(client, session.id);
      assertAddTurn(events, 3);
      pass('turn 2 (same session): add 1 2 -> result 3');

      // --- the fixture saw the admin-authored secret as the bearer ---
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

    console.log('E2E PASS: management-plane MCP config drives a multi-turn MCP conversation with the vault-backed bearer.');
    process.exitCode = 0;
  } catch (err) {
    console.error('E2E FAIL:', err);
    process.exitCode = 1;
  } finally {
    await fixture.close();
  }
}

main();
