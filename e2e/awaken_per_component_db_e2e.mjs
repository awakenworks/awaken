// Per-component database isolation for the aggregated `awaken` command (Serve role).
//
// The control plane owns several stores (catalog, credential vault, config, admin,
// sessions). This test proves each can be pointed at its OWN database independently
// via typed deployment fields, then still resolves + runs the configured model. It
// boots `awaken` with one data root plus catalog/credential/config redirected to a
// SEPARATE directory, authors the model there, runs a session over a
// fake upstream, and asserts the stores physically landed where configured — the
// redirected files exist in the other dir, and NOT in the bundle dir (admin/sessions
// stay in the bundle). This is the precondition for splitting control / server into
// separate services that share per-component databases.
//
// Run: (from e2e/)  npm install && node awaken_per_component_db_e2e.mjs

import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import Anthropic from '@anthropic-ai/sdk';
import { startFakeAnthropic } from './fixtures/fake_anthropic_fixture.mjs';
import { spawnProduction, stopServer, waitForPort } from './harness.mjs';

const PORT = Number(process.env.E2E_PORT ?? 38421);
const BETAS = ['managed-agents-2026-04-01'];
const FAKE_KEY = 'sk-awaken-percomp-fake-key'; // awaken-allow: secret
const SEAL_KEY = '00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff';
const WORKSPACE = 'wrkspc_default';
const AGENT = 'db-model-agent';
const MODEL = 'fake-haiku';
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

async function ready(base, timeoutMs = 60_000) {
  const deadline = Date.now() + timeoutMs;
  for (;;) {
    try {
      if ((await fetch(`${base}/v1/capabilities`)).ok) return;
    } catch {
      /* not up */
    }
    if (Date.now() > deadline) throw new Error('management plane not ready');
    await sleep(200);
  }
}

async function req(base, method, uri, body) {
  const res = await fetch(`${base}${uri}`, {
    method,
    headers: body === undefined ? {} : { 'content-type': 'application/json' },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  const text = await res.text();
  let json = null;
  try {
    json = text ? JSON.parse(text) : null;
  } catch {
    json = { _raw: text };
  }
  return { status: res.status, json };
}

async function main() {
  const upstream = await startFakeAnthropic(FAKE_KEY);
  const bundle = fs.mkdtempSync(path.join(os.tmpdir(), 'awaken-bundle-'));
  const other = fs.mkdtempSync(path.join(os.tmpdir(), 'awaken-other-'));
  const catalogDb = path.join(other, 'my-catalog.db');
  const credentialDb = path.join(other, 'secure', 'my-credential.db'); // nested: dir is created
  const configDb = path.join(other, 'my-config.db');

  const server = spawnProduction(bundle, PORT, {
    controlSealKey: SEAL_KEY,
    databases: {
      catalog_db: catalogDb,
      credential_db: credentialDb,
      config_db: configDb,
    },
  });

  const base = `http://127.0.0.1:${PORT}`;
  try {
    await waitForPort(PORT, 60_000, server);
    await ready(base);
    console.log('ok: awaken booted with typed per-component database configuration');

    // The single connection command atomically authors the catalog and vault.
    let r = await req(base, 'POST', '/v1/config/provider-connections', {
      workspace_id: WORKSPACE,
      provider_id: 'anthropic',
      display_name: 'Anthropic',
      dialect: 'anthropic_messages',
      base_url: `${upstream.url}/v1/`,
      timeout_secs: 300,
      secret: FAKE_KEY,
    });
    assert.equal(r.status, 201, `provider connection: ${JSON.stringify(r.json)}`);
    // Publish an agent bound to the model (lands in the redirected config DB).
    r = await req(base, 'PUT', `/v1/config/agents/${AGENT}`, {
      name: AGENT, model: { id: MODEL }, system: 'test', max_steps: 2,
    });
    assert.equal(r.status, 200, `agent: ${JSON.stringify(r.json)}`);
    r = await req(base, 'POST', `/v1/config/agents/${AGENT}/publish`, undefined);
    assert.equal(r.status, 200, `publish: ${JSON.stringify(r.json)}`);
    console.log('ok: connected provider + published agent');

    // Each redirected store physically landed at its configured path...
    assert.ok(fs.existsSync(catalogDb), `catalog db at ${catalogDb}`);
    assert.ok(fs.existsSync(credentialDb), `credential db at nested ${credentialDb}`);
    assert.ok(fs.existsSync(configDb), `config db at ${configDb}`);
    // ...and NOT in the bundle dir (they were redirected out of it).
    assert.ok(!fs.existsSync(path.join(bundle, 'catalog.db')), 'catalog is not in the bundle');
    assert.ok(!fs.existsSync(path.join(bundle, 'credential.db')), 'credential is not in the bundle');
    assert.ok(!fs.existsSync(path.join(bundle, 'config.db')), 'config is not in the bundle');
    // The un-redirected components stay in the bundle.
    assert.ok(fs.existsSync(path.join(bundle, 'admin.db')), 'admin stays in the bundle');
    assert.ok(fs.existsSync(path.join(bundle, 'sessions.db')), 'sessions stays in the bundle');
    console.log('ok: catalog/credential/config isolated to their own DBs; admin/sessions in the bundle');

    // The split stores still resolve + run the configured model over the wire.
    const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: base });
    const session = await client.beta.sessions.create({ agent: AGENT, environment_id: 'env_local', betas: BETAS });
    await client.beta.sessions.events.send(session.id, {
      events: [{ type: 'user.message', content: [{ type: 'text', text: 'resolve me' }] }],
      betas: BETAS,
    });
    const events = [];
    for await (const ev of client.beta.sessions.events.list(session.id, { betas: BETAS })) events.push(ev);
    const msg = events.find((e) => e.type === 'agent.message');
    const text = (msg?.content ?? []).map((c) => c.text ?? '').join('');
    assert.ok(text.includes('FAKE:resolve me'), `ran the configured model across split DBs: ${JSON.stringify(text)}`);
    assert.ok(upstream.requests.length >= 1, 'fake upstream received the configured-model call');
    console.log('ok: model resolved + ran across per-component split databases');
  } finally {
    await stopServer(server);
    upstream.close();
    fs.rmSync(bundle, { recursive: true, force: true });
    fs.rmSync(other, { recursive: true, force: true });
  }
  console.log('\nawaken_per_component_db_e2e: PASS');
}

main().catch((err) => {
  console.error(err);
  process.exit(1);
});
