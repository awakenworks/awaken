// Webhook e2e (ADR-0048 / S10) for the OPEN single-machine binary: register a
// subscription over the real CRUD route, create a session with the official
// Anthropic SDK, and verify the delivered payload with the REAL `standardwebhooks`
// library — cross-language proof that the Rust signer and a stock verifier agree.
//
// The guard resolves the owning workspace from the API key, the lifecycle sink
// fans `session.status_idle` out signed over real HTTP, the local receiver checks
// the signature and the `workspace_id` stamping. No mocks in the transport.
//
// Run: (from e2e/)  npm install && node webhook_e2e.mjs

import assert from 'node:assert/strict';
import fs from 'node:fs';
import http from 'node:http';
import net from 'node:net';
import os from 'node:os';
import path from 'node:path';
import readline from 'node:readline';
import { spawn, execSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import Anthropic from '@anthropic-ai/sdk';
import { Webhook } from 'standardwebhooks';

const REPO_ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const PORT = Number(process.env.E2E_PORT ?? 38361);
const BETAS = ['managed-agents-2026-04-01'];
const WORKSPACE = 'wrkspc_local'; // the standalone's seeded singleton workspace
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

function standaloneBin() {
  const out = execSync(
    'cargo build --quiet --message-format=json -p awaken-standalone --bin awaken-standalone',
    { cwd: REPO_ROOT, maxBuffer: 64 * 1024 * 1024 },
  ).toString();
  for (const line of out.split('\n')) {
    if (!line.trim()) continue;
    let msg;
    try { msg = JSON.parse(line); } catch { continue; }
    if (msg.executable && msg.target?.name === 'awaken-standalone') return msg.executable;
  }
  throw new Error('could not resolve the awaken-standalone binary path');
}

function waitForPort(port, timeoutMs = 60_000) {
  const deadline = Date.now() + timeoutMs;
  return new Promise((resolve, reject) => {
    const attempt = () => {
      const sock = net.createConnection({ port, host: '127.0.0.1' });
      sock.once('connect', () => { sock.destroy(); resolve(); });
      sock.once('error', () => {
        sock.destroy();
        if (Date.now() > deadline) reject(new Error(`server did not listen on ${port}`));
        else setTimeout(attempt, 200);
      });
    };
    attempt();
  });
}

function startStandalone(bin, port, extraEnv) {
  const server = spawn(bin, {
    env: { ...process.env, AWAKEN_STANDALONE_ADDR: `127.0.0.1:${port}`, ...extraEnv },
    stdio: ['ignore', 'inherit', 'pipe'],
  });
  const keys = {};
  readline.createInterface({ input: server.stderr }).on('line', (line) => {
    process.stderr.write(`${line}\n`);
    const api = line.match(/api key:\s+(sk-awaken-\S+)/);
    if (api) keys.api = api[1];
    const admin = line.match(/admin key:\s+(sk-awaken-\S+)/);
    if (admin) keys.admin = admin[1];
  });
  const stop = () => new Promise((resolve) => {
    if (server.exitCode !== null) return resolve();
    server.on('exit', () => resolve());
    server.kill('SIGINT');
  });
  return { server, keys, baseUrl: `http://127.0.0.1:${port}`, stop };
}

// A local receiver that records the next webhook delivery (body + headers).
function startReceiver() {
  const inbox = [];
  const server = http.createServer((req, res) => {
    let body = '';
    req.on('data', (c) => (body += c));
    req.on('end', () => {
      inbox.push({ body, headers: req.headers });
      res.writeHead(200).end('ok');
    });
  });
  return new Promise((resolve) => {
    server.listen(0, '127.0.0.1', () => {
      const { port } = server.address();
      resolve({ url: `http://127.0.0.1:${port}/hook`, inbox, close: () => server.close() });
    });
  });
}

async function main() {
  const bin = standaloneBin();
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'awaken-webhook-e2e-'));
  const receiver = await startReceiver();
  const h = startStandalone(bin, PORT, { AWAKEN_WEBHOOK_DIR: dir });
  try {
    await waitForPort(PORT);
    for (let i = 0; i < 200 && !h.keys.api; i++) await sleep(50);
    assert.ok(h.keys.api && h.keys.admin, 'captured the seeded keys');

    // 1. Register a subscription over the real CRUD route (admin token).
    const reg = await fetch(`${h.baseUrl}/v1/workspaces/${WORKSPACE}/webhooks`, {
      method: 'POST',
      headers: { 'content-type': 'application/json', authorization: `Bearer ${h.keys.admin}` },
      body: JSON.stringify({ url: receiver.url, event_types: ['session.status_idle'] }),
    });
    assert.equal(reg.status, 201, `subscription create -> 201 (got ${reg.status})`);
    const { secret, id } = await reg.json();
    assert.ok(secret.startsWith('whsec_'), 'a whsec_ signing secret is returned once');
    console.log('  ok: subscription registered via /v1/workspaces/{ws}/webhooks');

    // 2. Create a session with the OFFICIAL Anthropic SDK (api token → the guard
    //    resolves its workspace); a fresh session is idle → the sink fans out.
    const sdk = new Anthropic({ apiKey: null, authToken: h.keys.api, baseURL: h.baseUrl });
    const session = await sdk.beta.sessions.create({ agent: 'assistant', betas: BETAS });
    assert.ok(session.id.startsWith('sesn_'), `session id: ${session.id}`);
    console.log('  ok: session created via the Anthropic SDK');

    // 3. Await the delivery.
    for (let i = 0; i < 150 && receiver.inbox.length === 0; i++) await sleep(20);
    assert.equal(receiver.inbox.length, 1, 'exactly one webhook was delivered');
    const { body, headers } = receiver.inbox[0];

    // 4. Verify with the REAL standardwebhooks library (throws on a bad signature).
    const wh = new Webhook(secret);
    const verified = wh.verify(body, {
      'webhook-id': headers['webhook-id'],
      'webhook-timestamp': headers['webhook-timestamp'],
      'webhook-signature': headers['webhook-signature'],
    });
    console.log('  ok: standardwebhooks verified the Rust-signed delivery');

    // 5. The payload is the Anthropic event shape, scoped to the workspace.
    assert.equal(verified.type, 'event');
    assert.equal(verified.data.type, 'session.status_idle');
    assert.equal(verified.data.id, session.id, 'the fact is about the created session');
    assert.equal(verified.data.workspace_id, WORKSPACE, 'stamped with the key-resolved workspace');
    assert.ok(verified.data.organization_id === undefined, 'self-hosted omits org');
    console.log(`  ok: event scoped to ${WORKSPACE} for ${session.id}`);

    // 6. DELETE unsubscribes.
    const del = await fetch(`${h.baseUrl}/v1/workspaces/${WORKSPACE}/webhooks/${id}`, {
      method: 'DELETE',
      headers: { authorization: `Bearer ${h.keys.admin}` },
    });
    assert.equal(del.status, 204, 'DELETE -> 204');
    console.log('  ok: unsubscribed via DELETE');

    console.log('E2E PASS: webhook plane — CRUD + Rust-signed delivery verified by standardwebhooks.');
  } finally {
    await h.stop();
    receiver.close();
  }
  process.exitCode = 0;
}

main().catch((err) => {
  console.error(`E2E FAIL: ${err?.stack || err}`);
  process.exitCode = 1;
});
