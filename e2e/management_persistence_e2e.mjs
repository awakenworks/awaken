// Restart-persistence e2e for the durable management plane (ADR-0043): spawn
// awaken-server in `management` mode with AWAKEN_MGMT_DIR + a fixed
// AWAKEN_MGMT_SEAL_KEY, author config through `/v1/config/*` AND enter an
// `mcp_oauth` credential through the official Anthropic SDK's vault front door
// (`beta.vaults.*`), kill the process, respawn it over the same dir/key, and
// assert exactly the documented persistence contract:
//
//   - DOMAIN state persists: catalog, secret-free credential rows (the SDK-entered
//     vault credential included), pool, inference profile, MCP server def,
//     agent↔MCP binding — and an MCP conversation still works, i.e. the sealed
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
import { spawnServer, stopServer, waitForPort, pass, startUpstream, realServerEnv } from './harness.mjs';
import { startCalcFixture } from './fixtures/mcp_calc_fixture.mjs';

const BETAS = ['managed-agents-2026-04-01'];
const PORT = 38195;
const CALC_TOKEN = 'calc-persist-bearer-token'; // awaken-allow: secret
// 64 hex chars = the 32-byte AEAD key AWAKEN_MGMT_SEAL_KEY requires.
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

async function main() {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'awaken-mgmt-e2e-'));
  const env = { AWAKEN_MGMT_DIR: dir, AWAKEN_MGMT_SEAL_KEY: SEAL_KEY };
  const fixture = await startCalcFixture(CALC_TOKEN);
  const upstream = await startUpstream('mcp');
  let server = null;
  try {
    // ---- lifetime A: author everything ------------------------------------
    let { server: a, baseUrl: base } = spawnServer('management', PORT, { ...env, ...realServerEnv('mcp', upstream, { mode: 'management' }) });
    server = a;
    await waitForPort(PORT);
    const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: base });

    // Catalog: provider / endpoint / offering.
    let r = await req(base, 'PUT', '/v1/config/providers/anthropic', {
      id: 'anthropic', slug: 'anthropic', display_name: 'Anthropic', version: 1,
    });
    assert.equal(r.status, 200);
    r = await req(base, 'PUT', '/v1/config/endpoints/ep1', {
      id: 'ep1', provider_id: 'anthropic', flavor: 'anthropic_messages',
      base_url: 'https://api.anthropic.com/v1/', timeout_secs: 300, display_name: 'prod', version: 1,
    });
    assert.equal(r.status, 200);
    r = await req(base, 'POST', '/v1/config/offerings', {
      model_id: 'claude-opus-4-8', provider_id: 'anthropic',
      protocol_endpoint_id: 'ep1', flavor: 'anthropic_messages', upstream_model: null,
    });
    assert.equal(r.status, 200);
    pass('authored provider/endpoint/offering under AWAKEN_MGMT_DIR');

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

    // The domain row the SDK entry created (workspace = the wire vault id).
    r = await req(base, 'GET', `/v1/config/credentials?workspace_id=${vault.id}`);
    assert.equal(r.status, 200);
    assert.equal(r.json.length, 1, JSON.stringify(r.json));
    const credId = r.json[0].id;
    pass(`SDK vault mcp_oauth credential entered -> domain row ${credId} (secret-free)`);

    // Admin aggregates: pool + profile + MCP def (Exact-bound to the SDK-entered
    // credential) + agent binding.
    r = await req(base, 'PUT', '/v1/config/credential-pools/pool1', {
      id: 'pool1', workspace_id: vault.id,
      members: [{ credential_source_id: credId, ordinal: 0, enabled: true, selection_weight: 0 }],
    });
    assert.equal(r.status, 200);
    r = await req(base, 'PUT', '/v1/config/inference-profiles/prof1', {
      model_id: 'claude-opus-4-8',
      credential_binding: { type: 'exact', credential_source_id: credId },
      disabled_endpoint_ids: [],
    });
    assert.equal(r.status, 200);
    r = await req(base, 'PUT', '/v1/config/mcp-servers/calc-def', {
      id: 'calc-def', display_name: 'calc', url: fixture.url,
      credential_binding: { type: 'exact', credential_source_id: credId }, version: 1,
    });
    assert.equal(r.status, 200);
    r = await req(base, 'PUT', '/v1/config/agents/calc-agent/mcp', {
      agent_id: 'calc-agent', mcp_server_ids: ['calc-def'], version: 1,
    });
    assert.equal(r.status, 200);
    pass('authored pool + profile + mcp-server def + agent binding');

    // ---- restart: kill the process, respawn over the same dir + key -------
    await stopServer(server);
    server = null;
    ({ server, baseUrl: base } = spawnServer('management', PORT, { ...env, ...realServerEnv('mcp', upstream, { mode: 'management' }) }));
    await waitForPort(PORT);
    const client2 = new Anthropic({ apiKey: 'e2e-dummy', baseURL: base });
    pass('server killed and respawned on the same port with the same AWAKEN_MGMT_DIR/key');

    // The vault WIRE object is host-ephemeral: gone after the restart (correct).
    const gone = await client2.beta.vaults
      .retrieve(vault.id, { betas: BETAS })
      .then(() => null, (err) => err);
    assert.equal(gone?.status, 404, `vault wire object should 404 after restart, got ${gone}`);
    pass('vault wire object 404s after restart (VaultState is host-ephemeral by design)');

    // The DOMAIN state persisted: every admin GET returns the authored object.
    r = await req(base, 'GET', '/v1/config/catalog');
    assert.equal(r.status, 200);
    assert.ok('anthropic' in r.json.providers && 'ep1' in r.json.endpoints);

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
    assert.equal(r.json.model_id, 'claude-opus-4-8');

    r = await req(base, 'GET', '/v1/config/mcp-servers/calc-def');
    assert.equal(r.status, 200);
    assert.equal(r.json.url, fixture.url);

    r = await req(base, 'GET', '/v1/config/agents/calc-agent/mcp');
    assert.equal(r.status, 200);
    assert.deepEqual(r.json.mcp_server_ids, ['calc-def']);
    pass('catalog + credential + pool + profile + mcp def + agent binding all persisted');

    // The sealed secret survived too: the persisted binding still resolves...
    r = await req(base, 'POST', '/v1/config/agents/calc-agent/mcp/resolve', { workspace_id: vault.id });
    assert.equal(r.status, 200, JSON.stringify(r.json));
    assert.equal(r.json[0].credential_present, true);
    pass('post-restart mcp resolve: credential_present=true from the sealed store');

    // ...and an MCP conversation works through the ADMIN-authored path (no
    // inline mcp_servers), with the PERSISTED credential as the bearer.
    const before = fixture.calls.filter((c) => c.method === 'tools/call').length;
    const session = await client2.beta.sessions.create({ agent: 'calc-agent', betas: BETAS });
    await client2.beta.sessions.events.send(session.id, {
      events: [{ type: 'user.message', content: [{ type: 'text', text: 'add 7 8' }] }],
      betas: BETAS,
    });
    const events = await listEvents(client2, session.id);
    assert.ok(
      events.some((e) => e.type === 'agent.tool_use' && e.name === 'mcp__calc__add'),
      `an mcp__calc__add tool_use: ${JSON.stringify(events.map((e) => e.type))}`,
    );
    assert.ok(
      events.some((e) => e.type === 'agent.message' && e.content[0].text.includes('result: 15')),
      'the conversation reports result: 15',
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
