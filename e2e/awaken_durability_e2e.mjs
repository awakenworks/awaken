// Durability + streaming for the aggregated `awaken` command (Serve role).
//
// Ported from the retired `standalone_e2e.mjs`: the deployment-agnostic properties
// it validated — the agent loop, an SSE stream, and a session surviving a full
// process restart — are properties of the shared open runtime, not of the (now
// deleted) `awaken-standalone` binary. Here they run through `awaken` Serve over a
// DB-configured model (fake Anthropic upstream), which is the surface that
// subsumed standalone.
//
// Not ported: standalone's boot-seeded two-key banner and its `/v1/sessions -> 401`
// assertion were specific to standalone's in-memory EnforceEngine guard. `awaken`
// Serve is open (key-resolved tenancy, no session-level 401) unless
// AWAKEN_MGMT_IAM=embedded; that authz stack has its own coverage and is out of
// scope here.
//
// Run: (from e2e/)  npm install && node awaken_durability_e2e.mjs

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
const BASE_PORT = Number(process.env.E2E_PORT ?? 38441);
const BETAS = ['managed-agents-2026-04-01'];
const FAKE_KEY = 'sk-awaken-durability-fake-key'; // awaken-allow: secret
const SEAL_KEY = '00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff';
const WORKSPACE = 'wrkspc_default';
const AGENT = 'durable-agent';
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

// Boot `awaken` (Serve) on `port` over a persistent bundle + storage dir, so a
// second boot over the SAME dirs is a real process restart. Returns the base URL
// and a `stop()` that resolves once the process has actually exited (dirs flushed).
function startAwaken(bin, port, dirs) {
  const server = spawn(bin, {
    env: {
      ...process.env,
      AWAKEN_HTTP_ADDR: `127.0.0.1:${port}`,
      AWAKEN_MGMT_DIR: dirs.bundle,
      AWAKEN_STORAGE_DIR: dirs.storage,
      AWAKEN_MGMT_SEAL_KEY: SEAL_KEY,
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
  return { baseUrl: `http://127.0.0.1:${port}`, stop };
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

// Author provider/endpoint/offering + credential and publish the agent bound to the
// model. Persists into the durable stores under AWAKEN_MGMT_DIR, so it survives the
// restart and the model still resolves on boot 2.
async function authorModel(base, upstream) {
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
  r = await req(base, 'PUT', `/v1/config/agents/${AGENT}`, {
    name: AGENT, model: { id: MODEL }, system: 'test', max_steps: 2,
  });
  assert.equal(r.status, 200, `agent: ${JSON.stringify(r.json)}`);
  r = await req(base, 'POST', `/v1/config/agents/${AGENT}/publish`, undefined);
  assert.equal(r.status, 200, `publish: ${JSON.stringify(r.json)}`);
}

function client(base) {
  return new Anthropic({ apiKey: 'e2e-dummy', baseURL: base });
}

// Send one user turn and return the agent's reply texts.
async function converse(sdk, sessionId, text) {
  await sdk.beta.sessions.events.send(sessionId, {
    events: [{ type: 'user.message', content: [{ type: 'text', text }] }],
    betas: BETAS,
  });
  const events = [];
  for await (const ev of sdk.beta.sessions.events.list(sessionId, { betas: BETAS })) events.push(ev);
  return events.filter((e) => e.type === 'agent.message').map((e) => (e.content ?? []).map((c) => c.text ?? '').join(''));
}

async function main() {
  const upstream = await startFakeAnthropic(FAKE_KEY);
  const bin = awakenBin();
  const dirs = {
    bundle: fs.mkdtempSync(path.join(os.tmpdir(), 'awaken-durable-mgmt-')),
    storage: fs.mkdtempSync(path.join(os.tmpdir(), 'awaken-durable-store-')),
  };

  let sessionId;
  // ── Boot 1: author the model, run one turn, and stream one turn ───────────────
  const first = startAwaken(bin, BASE_PORT, dirs);
  try {
    await waitForPort(BASE_PORT);
    await ready(first.baseUrl);
    await authorModel(first.baseUrl, upstream);
    console.log('ok: awaken booted (durable) + authored the DB-configured model');

    const sdk = client(first.baseUrl);
    const session = await sdk.beta.sessions.create({ agent: AGENT, environment_id: 'env_local', betas: BETAS });
    assert.ok(session.id.startsWith('sesn_'), `session id: ${session.id}`);
    sessionId = session.id;

    const replies = await converse(sdk, sessionId, 'hello');
    assert.ok(replies.some((t) => t.includes('FAKE:hello')), `pre-restart turn: ${JSON.stringify(replies)}`);
    console.log('ok: full agent turn on the configured model');

    // SSE stream carries the turn's events (events.stream), preserving standalone's
    // streaming coverage on the aggregated binary.
    await sdk.beta.sessions.events.send(sessionId, {
      events: [{ type: 'user.message', content: [{ type: 'text', text: 'stream me' }] }],
      betas: BETAS,
    });
    const stream = await sdk.beta.sessions.events.stream(sessionId, { betas: BETAS });
    const streamedTypes = [];
    for await (const ev of stream) streamedTypes.push(ev.type);
    assert.ok(streamedTypes.includes('agent.message'), `stream types: ${streamedTypes}`);
    console.log('ok: SSE stream (events.stream) carries the agent turn');
  } finally {
    await first.stop();
  }

  // ── Boot 2 over the SAME dirs: a fresh process continues the SAME session ──────
  // The session (sessions.db) and its transcript (commit store under
  // AWAKEN_STORAGE_DIR) rehydrate, and a new turn appends another reply — session
  // reachability after a full process death is the durability guarantee.
  //
  // Crucially, the post-restart turn resolves the DB-CONFIGURED model (`FAKE:`), not
  // the in-process seed model: on boot the server WARM-LOADS the installed catalog
  // from config.db, so a rehydrated session's published agent still resolves its
  // configured model across the restart (the config-durability fix).
  const second = startAwaken(bin, BASE_PORT + 1, dirs);
  try {
    await waitForPort(BASE_PORT + 1);
    await ready(second.baseUrl);
    const sdk = client(second.baseUrl);
    // (a) Config durability: a NEW session for the same agent resolves the
    //     DB-configured model (`FAKE:`), proving the installed catalog was
    //     warm-loaded from config.db on boot — not the seed model.
    const fresh = await sdk.beta.sessions.create({ agent: AGENT, environment_id: 'env_local', betas: BETAS });
    const freshReplies = await converse(sdk, fresh.id, 'after restart');
    assert.ok(
      freshReplies.some((t) => t.includes('FAKE:after restart')),
      `warm-load: a new session resolved the configured model after restart: ${JSON.stringify(freshReplies)}`,
    );
    console.log('ok: warm-load — a new session resolves the DB-configured model after restart');

    // (b) Session durability: the ORIGINAL session rehydrated its transcript from
    //     the durable commit store and continues (the faithful standalone_e2e claim).
    const replies = await converse(sdk, sessionId, 'again');
    assert.ok(replies.some((t) => t.includes('FAKE:hello')), `pre-restart transcript rehydrated: ${JSON.stringify(replies)}`);
    assert.ok(replies.length >= 3, `the original session continued after restart (${replies.length} replies): ${JSON.stringify(replies)}`);
    console.log('ok: the original session survived a full process restart and continued');
  } finally {
    await second.stop();
    upstream.close();
    fs.rmSync(dirs.bundle, { recursive: true, force: true });
    fs.rmSync(dirs.storage, { recursive: true, force: true });
  }
  console.log('\nawaken_durability_e2e: PASS');
}

main().catch((err) => {
  console.error(err);
  process.exit(1);
});
