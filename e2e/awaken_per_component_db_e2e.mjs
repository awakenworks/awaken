// Per-component database isolation for the aggregated `awaken` command (Serve role).
//
// The control plane owns several stores (catalog, credential vault, config, admin,
// sessions). This test proves each can be pointed at its OWN database independently
// via `AWAKEN_<COMPONENT>_DB`, then still resolves + runs the configured model. It
// boots `awaken` with a bundle `AWAKEN_MGMT_DIR` plus catalog/credential/config
// redirected to a SEPARATE directory, authors the model there, runs a session over a
// fake upstream, and asserts the stores physically landed where configured — the
// redirected files exist in the other dir, and NOT in the bundle dir (admin/sessions
// stay in the bundle). This is the precondition for splitting control / server into
// separate services that share per-component databases.
//
// Run: (from e2e/)  npm install && node awaken_per_component_db_e2e.mjs

import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import net from 'node:net';
import path from 'node:path';
import readline from 'node:readline';
import { spawn, execSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import Anthropic from '@anthropic-ai/sdk';
import { startFakeAnthropic } from './fixtures/fake_anthropic_fixture.mjs';

const REPO_ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const PORT = Number(process.env.E2E_PORT ?? 38421);
const BETAS = ['managed-agents-2026-04-01'];
const FAKE_KEY = 'sk-awaken-percomp-fake-key'; // awaken-allow: secret
const SEAL_KEY = '00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff';
const WORKSPACE = 'wrkspc_default';
const AGENT = 'db-model-agent';
const MODEL = 'fake-haiku';
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

function awakenBin() {
  const out = execSync('cargo build --quiet --message-format=json -p awaken-cli --bin awaken', {
    cwd: REPO_ROOT,
    maxBuffer: 64 * 1024 * 1024,
  }).toString();
  for (const line of out.split('\n')) {
    if (!line.trim()) continue;
    let msg;
    try {
      msg = JSON.parse(line);
    } catch {
      continue;
    }
    if (msg.executable && msg.target?.name === 'awaken') return msg.executable;
  }
  throw new Error('could not resolve the awaken binary path');
}

function waitForPort(port, timeoutMs = 60_000) {
  const deadline = Date.now() + timeoutMs;
  return new Promise((resolve, reject) => {
    const attempt = () => {
      const sock = net.createConnection({ port, host: '127.0.0.1' });
      sock.once('connect', () => {
        sock.destroy();
        resolve();
      });
      sock.once('error', () => {
        sock.destroy();
        if (Date.now() > deadline) reject(new Error(`server did not listen on ${port}`));
        else setTimeout(attempt, 200);
      });
    };
    attempt();
  });
}

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
  const bin = awakenBin();
  const bundle = fs.mkdtempSync(path.join(os.tmpdir(), 'awaken-bundle-'));
  const other = fs.mkdtempSync(path.join(os.tmpdir(), 'awaken-other-'));
  const catalogDb = path.join(other, 'my-catalog.db');
  const credentialDb = path.join(other, 'secure', 'my-credential.db'); // nested: dir is created
  const configDb = path.join(other, 'my-config.db');

  const server = spawn(bin, {
    env: {
      ...process.env,
      AWAKEN_HTTP_ADDR: `127.0.0.1:${PORT}`,
      AWAKEN_MGMT_DIR: bundle,
      AWAKEN_MGMT_SEAL_KEY: SEAL_KEY,
      // Redirect three components out of the bundle, each to its own file.
      AWAKEN_CATALOG_DB: catalogDb,
      AWAKEN_CREDENTIAL_DB: credentialDb,
      AWAKEN_CONFIG_DB: configDb,
    },
    stdio: ['ignore', 'inherit', 'pipe'],
  });
  readline.createInterface({ input: server.stderr }).on('line', (l) => process.stderr.write(`${l}\n`));
  const stop = () =>
    new Promise((resolve) => {
      if (server.exitCode !== null) return resolve();
      server.on('exit', () => resolve());
      server.kill('SIGINT');
    });

  const base = `http://127.0.0.1:${PORT}`;
  try {
    await waitForPort(PORT);
    await ready(base);
    console.log('ok: awaken booted with per-component AWAKEN_*_DB overrides');

    // Author the model in the (redirected) catalog + credential DBs.
    let r = await req(base, 'PUT', '/v1/config/providers/anthropic', {
      id: 'anthropic', slug: 'anthropic', display_name: 'Anthropic', version: 1,
    });
    assert.equal(r.status, 200, `provider: ${JSON.stringify(r.json)}`);
    r = await req(base, 'PUT', '/v1/config/endpoints/ep1', {
      id: 'ep1', provider_id: 'anthropic', dialect: 'anthropic_messages',
      base_url: `${upstream.url}/v1/`, timeout_secs: 300, display_name: 'fake', version: 1,
    });
    assert.equal(r.status, 200, `endpoint: ${JSON.stringify(r.json)}`);
    r = await req(base, 'POST', '/v1/config/offerings', {
      model_id: MODEL, provider_id: 'anthropic',
      protocol_endpoint_id: 'ep1', dialect: 'anthropic_messages', upstream_model: null,
    });
    assert.equal(r.status, 200, `offering: ${JSON.stringify(r.json)}`);
    r = await req(base, 'POST', '/v1/config/credentials', {
      workspace_id: WORKSPACE, kind: 'vault', provider_id: 'anthropic',
      env_key: 'ANTHROPIC_API_KEY', secret: FAKE_KEY,
    });
    assert.equal(r.status, 201, `credential: ${JSON.stringify(r.json)}`);
    // Publish an agent bound to the model (lands in the redirected config DB).
    r = await req(base, 'PUT', `/v1/config/agents/${AGENT}`, {
      name: AGENT, model: { id: MODEL }, system: 'test', max_steps: 2,
    });
    assert.equal(r.status, 200, `agent: ${JSON.stringify(r.json)}`);
    r = await req(base, 'POST', `/v1/config/agents/${AGENT}/publish`, undefined);
    assert.equal(r.status, 200, `publish: ${JSON.stringify(r.json)}`);
    console.log('ok: authored provider/offering/credential + published agent');

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
    await stop();
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
