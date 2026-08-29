// Management-plane MCP e2e: the whole agent↔MCP binding is authored through the
// ADMIN config surface (`/v1/config/*`, validated against the generated TS API
// contract with Ajv, like management_admin_e2e.mjs) — credential (secret-in) →
// typed AgentConfig MCP binding with an exact CredentialRef — then a
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
import { withScenarioServer, pass, waitForSessionEventReceipt } from './harness.mjs';
import { startCalcFixture } from './fixtures/mcp_calc_fixture.mjs';
import { alwaysAllowMcpTools } from './fixtures/managed_mcp_session.ts';

const REPO_ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const CONTRACT = JSON.parse(
  fs.readFileSync(path.join(REPO_ROOT, 'contracts', 'model-schemas.generated.json'), 'utf8'),
);
const ajv = new Ajv2020({ strict: false, allErrors: true });
const validators = Object.fromEntries(
  Object.entries(CONTRACT.schemas).map(([name, schema]) => [name, ajv.compile(schema)]),
);

const BETAS = ['managed-agents-2026-04-01'];
const CALC_TOKEN = 'calc-mgmt-bearer-token'; // awaken-allow: secret

function mcpBearerDescriptor(audience) {
  return {
    provider: audience,
    material: { kind: 'secret', type_id: 'awaken.secret/v1' },
    targets: [{
      target: { purpose: { type: 'mcp_authorization' }, audience },
      usage: { type: 'http_header', name: 'authorization', scheme: 'Bearer' },
    }],
  };
}

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

async function sendMessage(client, sessionId, text, predicate, description) {
  // Managed-MCP turn rule M0: C1=one exact User receipt and C2=the caller's
  // add/permission effect; E1=return full history plus the post-C1 delta.
  // Constraint: older turns cannot satisfy C2. C1&&!C2=>observe; C1+C2=>E1.
  const receipt = await client.beta.sessions.events.send(sessionId, {
    events: [{ type: 'user.message', content: [{ type: 'text', text }] }],
    betas: BETAS,
  });
  return waitForSessionEventReceipt(
    client,
    sessionId,
    receipt.data[0]?.id,
    BETAS,
    predicate,
    description,
  );
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
      // Credential decision rule C0: the canonical MCP audience plus Bearer
      // usage authorizes only this target; an undescribed source must fail
      // before plaintext materialization. The fixture URL's cosmetic trailing
      // slash is removed by the same identity rule used by the MCP aggregate.
      const calcAudience = fixture.url.replace(/\/+$/u, '');
      let r = await req(base, 'POST', '/v1/config/credentials', {
        idempotency_key: 'management-mcp-primary',
        workspace_id: 'ws',
        kind: 'vault',
        secret: CALC_TOKEN,
        descriptor: mcpBearerDescriptor(calcAudience),
      });
      assert.equal(r.status, 201, JSON.stringify(r.json));
      checkContract('CredentialSource', r.json);
      assert.ok(!JSON.stringify(r.json).includes(CALC_TOKEN), 'secret must not be echoed');
      const credId = r.json.id;
      const credRevision = r.json.version;
      pass('POST /v1/config/credentials — secret-free CredentialSource matches contract');

      const calcServer = {
        name: 'calc',
        url: fixture.url,
        credential: { id: credId, revision: credRevision },
      };
      r = await req(base, 'PUT', '/v1/config/agents/calc-agent', {
        name: 'Calculator',
        system: 'Use the calculator tool and report its result.',
        model: {
          mode: 'pinned',
          provider_identity_ref: 'default',
          model_ref: 'management',
          backend_ref: 'default',
        },
        mcp_servers: [calcServer],
        tools: alwaysAllowMcpTools([calcServer]),
      });
      assert.equal(r.status, 200, JSON.stringify(r.json));
      r = await req(base, 'GET', '/v1/config/agents/calc-agent');
      assert.deepEqual(r.json.mcp_servers[0].credential, {
        id: credId,
        revision: credRevision,
      });
      r = await req(base, 'POST', '/v1/config/agents/calc-agent/publish');
      assert.equal(r.status, 200, JSON.stringify(r.json));
      assert.ok(r.json.fingerprint, 'publication carries immutable identity');
      assert.ok(!JSON.stringify(r.json).includes(CALC_TOKEN), 'publication is secret-free');
      pass('typed Agent MCP binding published with an exact secret-free credential revision');

      // --- a session for calc-agent with NO inline mcp_servers ---
      const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: base });
      const session = await client.beta.sessions.create({
        agent: {
          id: 'calc-agent',
          type: 'agent_with_overrides',
          tools: alwaysAllowMcpTools([{ name: 'calc', type: 'url', url: fixture.url }]),
        },
        environment_id: 'env_local',
        betas: BETAS,
      });
      assert.equal(session.type, 'session');
      assert.equal(session.agent.id, 'calc-agent');

      let observed = await sendMessage(
        client,
        session.id,
        'add 7 8',
        ({ delta }) => agentMessages(delta).some((message) => message.includes('result: 15')),
        'management MCP turn 1 commits result 15',
      );
      assertAddTurn(observed.delta, 15);
      pass('turn 1: add 7 8 -> mcp__calc__add via the admin-authored config, result 15');

      observed = await sendMessage(
        client,
        session.id,
        'add 1 2',
        ({ delta }) => agentMessages(delta).some((message) => message.includes('result: 3')),
        'management MCP turn 2 commits result 3',
      );
      assertAddTurn(observed.delta, 3);
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

      // Anthropic MCP toolset default is always_ask. The permission policy is
      // carried through the neutral AgentConfigView/SessionInit into the one
      // runtime gate, then projected as the same Managed event sequence.
      //
      // FMECA cause/effect graph: C1=MCP toolset owns policy; C2=tool call is
      // enabled and evaluates `always_ask`; C3=an exact out-of-band decision is
      // supplied. Effects: E1=without C3 the Run remains `requires_action` and
      // performs no `tools/call`; E2=allow executes exactly once and emits an MCP
      // result; E3=deny emits a blocked MCP result for the model but performs no
      // transport call. Competing legacy policy ownership fails at compilation.
      //
      // | Rule | C1 | C2 | C3      | Effect |
      // |---|---|---|---|---|
      // | O1 | T | T | absent | E1 durable await, no call |
      // | O2 | T | T | allow  | E2 one call + result |
      // | O3 | T | T | deny   | E3 blocked result, no call |
      // | O4 | T + legacy | * | * | reject competing owners |
      //
      // Compile-level tests own O4. This served scenario owns O1-O3 through the
      // same durable permission/resume path used by built-ins; MCP adds no
      // parallel approval authority.
      const gatedAgent = 'calc-gated-agent';
      r = await req(base, 'PUT', `/v1/config/agents/${gatedAgent}`, {
        name: 'Gated Calculator',
        system: 'Use the calculator tool and report its result.',
        model: { mode: 'pinned', provider_identity_ref: 'default', model_ref: 'management', backend_ref: 'default' },
        mcp_servers: [{ name: 'calc', url: fixture.url, credential: { id: credId, revision: credRevision } }],
        tools: [{ type: 'mcp_toolset', mcp_server_name: 'calc', default_config: { enabled: true, permission_policy: { type: 'always_ask' } } }],
      });
      assert.equal(r.status, 200, JSON.stringify(r.json));
      r = await req(base, 'POST', `/v1/config/agents/${gatedAgent}/publish`);
      assert.equal(r.status, 200, JSON.stringify(r.json));
      const gated = await client.beta.sessions.create({
        agent: gatedAgent,
        environment_id: 'env_local',
        betas: BETAS,
      });
      let gatedObservation = await sendMessage(
        client,
        gated.id,
        'add 9 4',
        ({ delta }) => delta.some((event) => event.type === 'agent.mcp_tool_use')
          && delta.some((event) => event.type === 'session.status_idle'
            && event.stop_reason?.type === 'requires_action'),
        'O1 exact receipt reaches MCP requires_action',
      );
      let gatedEvents = gatedObservation.delta;
      const gatedUse = gatedEvents.find((event) => event.type === 'agent.mcp_tool_use');
      assert.ok(gatedUse, `MCP tool call should be parked: ${gatedEvents.map((event) => event.type)}`);
      const gatedIdle = [...gatedEvents].reverse().find((event) => event.type === 'session.status_idle');
      assert.equal(gatedIdle.stop_reason.type, 'requires_action');
      const allowReceipt = await client.beta.sessions.events.send(gated.id, {
        events: [{ type: 'user.tool_confirmation', tool_use_id: gatedUse.id, result: 'allow' }],
        betas: BETAS,
      });
      gatedObservation = await waitForSessionEventReceipt(
        client,
        gated.id,
        allowReceipt.data[0]?.id,
        BETAS,
        ({ delta }) => delta.some((event) => event.type === 'agent.mcp_tool_result')
          && delta.some((event) => event.type === 'agent.message'),
        'O2 exact allow receipt reaches MCP result and terminal message',
      );
      gatedEvents = gatedObservation.delta;
      assert.ok(gatedEvents.some((event) => event.type === 'agent.mcp_tool_result'), JSON.stringify(gatedEvents));
      pass('O1/O2: MCP always_ask -> requires_action -> allow -> one tools/call');

      const callsBeforeDeny = fixture.calls.filter((call) => call.method === 'tools/call').length;
      gatedObservation = await sendMessage(
        client,
        gated.id,
        'add 6 5',
        ({ delta }) => delta.some((event) => event.type === 'agent.mcp_tool_use')
          && delta.some((event) => event.type === 'session.status_idle'
            && event.stop_reason?.type === 'requires_action'),
        'O3 exact receipt reaches MCP requires_action',
      );
      gatedEvents = gatedObservation.delta;
      const deniedUse = gatedEvents
        .filter((event) => event.type === 'agent.mcp_tool_use')
        .at(-1);
      assert.ok(deniedUse, JSON.stringify(gatedEvents));
      const deniedIdle = [...gatedEvents]
        .reverse()
        .find((event) => event.type === 'session.status_idle');
      assert.equal(deniedIdle.stop_reason.type, 'requires_action');
      assert.ok(deniedIdle.stop_reason.event_ids.includes(deniedUse.id));
      const denyReceipt = await client.beta.sessions.events.send(gated.id, {
        events: [{
          type: 'user.tool_confirmation',
          tool_use_id: deniedUse.id,
          result: 'deny',
          deny_message: 'operator denied MCP access',
        }],
        betas: BETAS,
      });
      gatedObservation = await waitForSessionEventReceipt(
        client,
        gated.id,
        denyReceipt.data[0]?.id,
        BETAS,
        ({ delta }) => delta.some((event) => event.type === 'agent.mcp_tool_result'
          && event.mcp_tool_use_id === deniedUse.id)
          && agentMessages(delta).some((message) =>
            message.includes('blocked: operator denied MCP access')),
        'O3 exact deny receipt reaches blocked MCP result and terminal message',
      );
      gatedEvents = gatedObservation.delta;
      const callsAfterDeny = fixture.calls.filter((call) => call.method === 'tools/call').length;
      assert.equal(callsAfterDeny, callsBeforeDeny, 'O3 deny must not reach MCP transport');
      const deniedResult = gatedEvents.find(
        (event) => event.type === 'agent.mcp_tool_result' &&
          event.mcp_tool_use_id === deniedUse.id,
      );
      assert.ok(deniedResult, JSON.stringify(gatedEvents));
      assert.match(JSON.stringify(deniedResult.content), /blocked: operator denied MCP access/u);
      assert.ok(
        agentMessages(gatedEvents).some((message) =>
          message.includes('blocked: operator denied MCP access')),
        'O3 model receives the blocked result and terminates the continuation',
      );
      pass('O3: MCP deny produces a model-visible blocked result without tools/call');

      // Exact-generation staging establishes the MCP connection before durable
      // activation.  It therefore rejects an invalid credential or unreachable
      // target at Session creation instead of publishing a Session whose MCP
      // projection cannot be realized.
      //
      // Cause-effect graph:
      // C1 target reachable + C2 credential accepted -> E1 activate Session
      // C1 target reachable + !C2 credential rejected -> E2 creation fails closed
      // !C1 target unreachable                    -> E3 retryable 503 before activation
      //
      // | Rule | C1 reachable | C2 accepted | Result                    |
      // | F1   | yes          | yes         | active Session            |
      // | F2   | yes          | no          | reject before activation  |
      // | F3   | no           | -           | 503 before activation     |
      const wrong = await req(base, 'POST', '/v1/config/credentials', {
        idempotency_key: 'management-mcp-wrong',
        workspace_id: 'ws',
        kind: 'vault',
        secret: 'wrong-mcp-token', // awaken-allow: secret (deliberate auth-failure fixture)
        descriptor: mcpBearerDescriptor(calcAudience),
      });
      assert.equal(wrong.status, 201);
      const badAgent = 'calc-auth-failure-agent';
      r = await req(base, 'PUT', `/v1/config/agents/${badAgent}`, {
        name: 'Bad MCP Auth',
        system: 'Use the calculator tool.',
        model: { mode: 'pinned', provider_identity_ref: 'default', model_ref: 'management', backend_ref: 'default' },
        mcp_servers: [{ name: 'calc', url: fixture.url, credential: { id: wrong.json.id, revision: wrong.json.version } }],
        tools: alwaysAllowMcpTools([{ name: 'calc' }]),
      });
      assert.equal(r.status, 200, JSON.stringify(r.json));
      r = await req(base, 'POST', `/v1/config/agents/${badAgent}/publish`);
      assert.equal(r.status, 200, JSON.stringify(r.json));
      await assert.rejects(
        client.beta.sessions.create({
          agent: badAgent,
          environment_id: 'env_local',
          betas: BETAS,
        }),
        (error) => error?.status === 500 &&
          error?.message?.includes('auth challenge: HTTP 401') &&
          !error?.message?.includes('wrong-mcp-token'),
        'F2: rejected MCP credentials must fail creation without leaking material',
      );
      assert.ok(
        fixture.tokenRequests['wrong-mcp-token'] > 0, // awaken-allow: secret
        'F2: exact pinned credential reached only the MCP target during staging',
      );
      pass('F2: MCP 401 rejects the staged generation before Session activation');

      const offlineAgent = 'calc-connection-failure-agent';
      r = await req(base, 'PUT', `/v1/config/agents/${offlineAgent}`, {
        name: 'Offline MCP',
        system: 'Use the unavailable calculator tool.',
        model: { mode: 'pinned', provider_identity_ref: 'default', model_ref: 'management', backend_ref: 'default' },
        mcp_servers: [{ name: 'offline', url: 'http://127.0.0.1:1/mcp' }],
        tools: alwaysAllowMcpTools([{ name: 'offline' }]),
      });
      assert.equal(r.status, 200, JSON.stringify(r.json));
      r = await req(base, 'POST', `/v1/config/agents/${offlineAgent}/publish`);
      assert.equal(r.status, 200, JSON.stringify(r.json));
      await assert.rejects(
        client.beta.sessions.create({
          agent: offlineAgent,
          environment_id: 'env_local',
          betas: BETAS,
        }),
        (error) => error?.status === 503 && error?.message?.includes('offline'),
        'F3: an unreachable MCP target must fail creation before activation',
      );
      pass('F3: MCP connection failure rejects the staged generation before activation');

    }, { ANTHROPIC_MODEL: 'management' });

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
