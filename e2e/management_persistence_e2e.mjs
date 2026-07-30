// Cause graph (durable management restart):
//   C1 domain aggregate is authored before restart -> E1 durable row is restored
//   C2 secret is sealed with the same key          -> E2 credential materializes
//   C3 wire-only vault object is process-local     -> E3 vault wire GET returns 404
//   C4 restored Agent uses restored MCP binding    -> E4 authenticated tool call works
//   C5 session explicitly allows the MCP tool      -> E5 transport proof is not paused by HITL
//
// Decision table:
//   Rule  C1  C2  C3  C4  C5  Expected
//   T1    Y   -   -   -   -   E1 (catalog/pool/normalized profile/Agent)
//   T2    Y   Y   -   Y   Y   E2 + E4 + E5
//   T3    -   -   Y   -   -   E3
//
// Restart-persistence e2e for the durable management plane (ADR-0043): spawn
// awaken-server in `management` mode with a fixed typed data_dir + seal key,
// author config through `/v1/config/*` AND enter an
// `mcp_oauth` credential through the official Anthropic SDK's vault front door
// (`beta.vaults.*`), kill the process, respawn it over the same dir/key, and
// assert exactly the documented persistence contract:
//
//   - DOMAIN state persists: catalog, secret-free credential rows (the SDK-entered
//     vault credential included), pool, inference profile, typed Agent MCP
//     binding — and an MCP conversation still works, i.e. the sealed
//     access token materialized from SQLite after the restart.
//   - WIRE state is host-ephemeral by design: the vault wire object 404s after
//     the restart (VaultState is rebuilt per process) while the domain row it
//     entered is still resolvable through the admin-authored path.
//
// Run: (from e2e/)  npm install && node management_persistence_e2e.mjs

import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import Anthropic from '@anthropic-ai/sdk';
import { deploymentEnv, spawnServer, stopServer, waitForPort, pass, startUpstream, realServerEnv, FAKE_KEY } from './harness.mjs';
import { startCalcFixture } from './fixtures/mcp_calc_fixture.mjs';

const BETAS = ['managed-agents-2026-04-01'];
const PORT = 38195;
const CALC_TOKEN = 'calc-persist-bearer-token'; // awaken-allow: secret
// 64 hex chars = the 32-byte AEAD key typed control_seal_key requires.
const SEAL_KEY = '00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff';

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

function agentMessages(events) {
  return events
    .filter((event) => event.type === 'agent.message')
    .flatMap((event) => event.content ?? [])
    .filter((block) => block.type === 'text')
    .map((block) => block.text);
}

async function main() {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'awaken-mgmt-e2e-'));
  const env = deploymentEnv(dir, { controlSealKey: SEAL_KEY });
  const fixture = await startCalcFixture(CALC_TOKEN);
  const upstream = await startUpstream('mcp');
  let server = null;
  try {
    // ---- lifetime A: author everything ------------------------------------
    let { server: a, baseUrl: base } = spawnServer('management', PORT, { ...env, ...realServerEnv('mcp', upstream, { mode: 'management' }) });
    server = a;
    await waitForPort(PORT);
    const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: base });

    // The canonical connection command persists catalog and credential facts.
    let r = await req(base, 'POST', '/v1/config/provider-connections', {
      workspace_id: 'wrkspc_default',
      provider_id: 'anthropic',
      display_name: 'Anthropic',
      dialect: 'anthropic_messages',
      base_url: `${upstream.url}/v1/`,
      timeout_secs: 300,
      secret: FAKE_KEY,
    });
    assert.equal(r.status, 201);
    pass('connected provider under the typed data_dir');

    // A vault `mcp_oauth` credential through the OFFICIAL SDK: the wire vault
    // bookkeeping is host-ephemeral, but the domain row + sealed access token
    // land in the durable stores.
    const vault = await client.beta.vaults.create({ display_name: 'persist vault', betas: BETAS });
    const wireCred = await client.beta.vaults.credentials.create(vault.id, {
      type: 'mcp_oauth',
      mcp_server_url: fixture.url,
      access_token: CALC_TOKEN,
      betas: BETAS,
    });
    assert.equal(wireCred.auth.type, 'mcp_oauth');
    assert.ok(!JSON.stringify(wireCred).includes(CALC_TOKEN), 'wire credential is secret-free');

    // The domain row the SDK entry created. The wire vault id is only a container
    // id; the durable row is owned by the platform-resolved local workspace.
    r = await req(base, 'GET', `/v1/config/credentials?workspace_id=${vault.id}`);
    assert.equal(r.status, 200);
    assert.equal(r.json.length, 1, JSON.stringify(r.json));
    const credId = r.json[0].id;
    pass(`SDK vault mcp_oauth credential entered -> domain row ${credId} (secret-free)`);

    // Admin aggregates: pool + profile + one typed Agent definition whose MCP
    // binding freezes the exact SDK-entered credential revision.
    r = await req(base, 'PUT', '/v1/config/credential-pools/pool1', {
      id: 'pool1', workspace_id: vault.id,
      members: [{ credential_source_id: credId, ordinal: 0, enabled: true, selection_weight: 0 }],
    });
    assert.equal(r.status, 200);
    r = await req(base, 'PUT', '/v1/config/inference-profiles/prof1', {
      primary: {
        target: { model_id: 'claude-opus-4-8' },
        credential_binding: { type: 'exact', credential_source_id: credId },
      },
      disabled_endpoint_ids: [],
    });
    assert.equal(r.status, 200);
    r = await req(base, 'PUT', '/v1/config/agents/calc-agent', {
      name: 'Calculator',
      system: 'Use the calculator tool and report its result.',
      model: {
        mode: 'pinned',
        provider_identity_ref: 'default',
        model_ref: 'fake-haiku',
        backend_ref: 'default',
      },
      mcp_servers: [{
        name: 'calc',
        url: fixture.url,
        credential: { id: credId, revision: 1 },
      }],
      tools: [{
        type: 'mcp_toolset',
        mcp_server_name: 'calc',
        default_config: {
          enabled: true,
          permission_policy: { type: 'always_allow' },
        },
      }],
    });
    assert.equal(r.status, 200);
    r = await req(base, 'POST', '/v1/config/agents/calc-agent/publish');
    assert.equal(r.status, 200, JSON.stringify(r.json));
    pass('authored pool + profile + published typed Agent MCP binding');

    // ---- restart: kill the process, respawn over the same dir + key -------
    await stopServer(server);
    server = null;
    ({ server, baseUrl: base } = spawnServer('management', PORT, { ...env, ...realServerEnv('mcp', upstream, { mode: 'management' }) }));
    await waitForPort(PORT);
    const client2 = new Anthropic({ apiKey: 'e2e-dummy', baseURL: base });
    pass('server killed and respawned on the same port with the same typed data_dir/key');

    // The vault WIRE object is host-ephemeral: gone after the restart (correct).
    const gone = await client2.beta.vaults
      .retrieve(vault.id, { betas: BETAS })
      .then(() => null, (err) => err);
    assert.equal(gone?.status, 404, `vault wire object should 404 after restart, got ${gone}`);
    pass('vault wire object 404s after restart (VaultState is host-ephemeral by design)');

    // The DOMAIN state persisted: every admin GET returns the authored object.
    r = await req(base, 'GET', '/v1/config/catalog');
    assert.equal(r.status, 200);
    assert.ok(
      'anthropic' in r.json.providers
        && 'anthropic.anthropic_messages' in r.json.endpoints,
    );

    r = await req(base, 'GET', `/v1/config/credentials?workspace_id=${vault.id}`);
    assert.equal(r.status, 200);
    assert.equal(r.json.length, 1);
    assert.equal(r.json[0].id, credId);
    assert.ok(!JSON.stringify(r.json).includes(CALC_TOKEN), 'persisted rows stay secret-free');

    r = await req(base, 'GET', '/v1/config/credential-pools/pool1');
    assert.equal(r.status, 200);
    assert.equal(r.json.members.length, 1);

    r = await req(base, 'GET', '/v1/config/inference-profiles/prof1');
    assert.equal(r.status, 200);
    assert.equal(r.json.primary.target.model_id, 'claude-opus-4-8');
    assert.equal(r.json.primary.credential_binding.credential_source_id, credId);

    r = await req(base, 'GET', '/v1/config/agents/calc-agent');
    assert.equal(r.status, 200);
    assert.equal(r.json.mcp_servers[0].url, fixture.url);
    assert.deepEqual(r.json.mcp_servers[0].credential, { id: credId, revision: 1 });
    pass('catalog + credential + pool + profile + typed Agent binding all persisted');

    // ...and an MCP conversation works through the ADMIN-authored path (no
    // inline mcp_servers), with the PERSISTED credential as the bearer.
    const before = fixture.calls.filter((c) => c.method === 'tools/call').length;
    const session = await client2.beta.sessions.create({
      agent: {
        id: 'calc-agent',
        type: 'agent_with_overrides',
        tools: [{
          type: 'mcp_toolset',
          mcp_server_name: 'calc',
          default_config: {
            enabled: true,
            permission_policy: { type: 'always_allow' },
          },
        }],
      },
      betas: BETAS,
    });
    await client2.beta.sessions.events.send(session.id, {
      events: [{ type: 'user.message', content: [{ type: 'text', text: 'add 7 8' }] }],
      betas: BETAS,
    });
    const events = await listEvents(client2, session.id);
    assert.ok(
      events.some((e) => e.type === 'agent.mcp_tool_use' && e.name === 'mcp__calc__add'),
      `an mcp__calc__add tool_use: ${JSON.stringify(events.map((e) => e.type))}`,
    );
    const messages = agentMessages(events);
    assert.ok(
      messages.some((message) => message.includes('result: 15')),
      `the conversation reports result: 15 — messages=${JSON.stringify(messages)}, event types=${JSON.stringify(events.map((event) => event.type))}`,
    );
    const toolCalls = fixture.calls.filter((c) => c.method === 'tools/call');
    assert.equal(toolCalls.length, before + 1);
    assert.equal(toolCalls.at(-1).authorization, `Bearer ${CALC_TOKEN}`);
    assert.equal(fixture.unauthorized, 0, 'no request was ever rejected for missing auth');
    pass('post-restart MCP conversation works with the persisted sealed credential as bearer');

    console.log(
      'E2E PASS: authored config + sealed credentials survive a server restart; wire vault state is ephemeral as documented.',
    );
    process.exitCode = 0;
  } catch (err) {
    console.error('E2E FAIL:', err);
    process.exitCode = 1;
  } finally {
    if (server) await stopServer(server);
    await fixture.close();
    upstream.close();
    fs.rmSync(dir, { recursive: true, force: true });
  }
}

main();
