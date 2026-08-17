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
// typed `identity_mode = "self-managed"`; that authz stack has its own coverage and is out of
// scope here.
//
// Run: (from e2e/)  npm install && node awaken_durability_e2e.mjs

import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import Anthropic from '@anthropic-ai/sdk';
import { startFakeAnthropic } from './fixtures/fake_anthropic_fixture.mjs';
import { spawnProduction, stopServer, waitForPort } from './harness.mjs';

const BASE_PORT = Number(process.env.E2E_PORT ?? 38441);
const BETAS = ['managed-agents-2026-04-01'];
const FAKE_KEY = 'sk-awaken-durability-fake-key'; // awaken-allow: secret
const SEAL_KEY = '00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff';
const WORKSPACE = 'wrkspc_default';
const AGENT = 'durable-agent';
const MODEL = 'fake-haiku';
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

async function ready(base, timeoutMs = 180_000) {
  const deadline = performance.now() + timeoutMs;
  for (;;) {
    try {
      if ((await fetch(`${base}/v1/capabilities`)).ok) return;
    } catch {
      /* not up */
    }
    if (performance.now() > deadline) throw new Error('management plane not ready');
    await sleep(200);
  }
}

// Boot production `awaken all-in-one` from the one typed deployment source. Reusing
// the exact data root across boots proves process durability without restoring
// the retired management/storage environment-variable configuration path.
function startAwaken(port, dataDir) {
  const server = spawnProduction(dataDir, port, { controlSealKey: SEAL_KEY });
  return {
    baseUrl: `http://127.0.0.1:${port}`,
    server,
    stop: () => stopServer(server),
  };
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

// Connect the provider atomically and publish the agent bound to the discovered
// model. Both catalog and credential state survive the restart.
async function authorModel(base, upstream) {
  let r = await req(base, 'POST', '/v1/config/provider-connections', {
    idempotency_key: 'awaken-durability-provider-connection',
    workspace_id: WORKSPACE,
    provider_id: 'anthropic',
    display_name: 'Anthropic',
    dialect: 'anthropic_messages',
    base_url: `${upstream.url}/v1/`,
    timeout_secs: 300,
    secret: FAKE_KEY,
  });
  assert.equal(r.status, 201, `provider connection: ${JSON.stringify(r.json)}`);
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
  const upstream = await startFakeAnthropic(FAKE_KEY, { models: [MODEL] });
  const dataDir = fs.mkdtempSync(path.join(os.tmpdir(), 'awaken-durable-'));

  let sessionId;
  // ── Boot 1: author the model, run one turn, and stream one turn ───────────────
  const first = startAwaken(BASE_PORT, dataDir);
  try {
    await waitForPort(BASE_PORT, 180_000, first.server);
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
    const streamAbort = new AbortController();
    const streamTimeout = setTimeout(
      () => streamAbort.abort(new Error('events.stream did not close after a terminal replay')),
      30_000,
    );
    const streamedTypes = [];
    try {
      const stream = await sdk.beta.sessions.events.stream(
        sessionId,
        { betas: BETAS },
        { signal: streamAbort.signal },
      );
      for await (const ev of stream) streamedTypes.push(ev.type);
    } finally {
      clearTimeout(streamTimeout);
    }
    assert.ok(streamedTypes.includes('agent.message'), `stream types: ${streamedTypes}`);
    assert.ok(streamedTypes.includes('session.status_idle'), `stream types: ${streamedTypes}`);
    console.log('ok: SSE stream (events.stream) carries the agent turn');
  } finally {
    await first.stop();
  }

  // ── Boot 2 over the SAME dirs: a fresh process continues the SAME session ──────
  // The session (sessions.db) and its transcript (commit store under
  // SESSION_DEPLOYMENT_STORAGE_DIR) rehydrate, and a new turn appends another reply — session
  // reachability after a full process death is the durability guarantee.
  //
  // Crucially, the post-restart turn resolves the DB-CONFIGURED model (`FAKE:`), not
  // the in-process seed model: on boot the server WARM-LOADS the installed catalog
  // from config.db, so a rehydrated session's published agent still resolves its
  // configured model across the restart (the config-durability fix).
  const second = startAwaken(BASE_PORT + 1, dataDir);
  try {
    await waitForPort(BASE_PORT + 1, 180_000, second.server);
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
    fs.rmSync(dataDir, { recursive: true, force: true });
  }
  console.log('\nawaken_durability_e2e: PASS');
}

main().catch((err) => {
  console.error(err);
  process.exit(1);
});
