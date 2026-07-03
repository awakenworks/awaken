// Durable operations surface (slice E): the ADR-0009 follow-on verbs over HTTP —
// reconcile (ADR-0011), reap + dead-letters + purge (ADR-0015).
//
// The deep dead-letter / crash-recovery STATE MACHINE is proven deterministically
// at the store level (awaken-run-ingress `sqlite_dispatch` tests) on this exact
// SQLite stack — a crashed `running` dispatch cannot be produced without an actual
// process crash mid-execution, which is timing-dependent. What this e2e proves is
// the SERVER SURFACE: the verbs are wired to a thread's durable queue, return the
// right shape on a live durable thread, and FAIL CLOSED (400) when the server is
// not in durable mode.
//
// Run: (from e2e/)  node managed_durable_ops_e2e.mjs

import assert from 'node:assert/strict';
import fs from 'node:fs';
import Anthropic from '@anthropic-ai/sdk';
import { spawnServer, stopServer, waitForPort, pass } from './harness.mjs';

const PORT = Number(process.env.E2E_PORT ?? 38177);
const BASE = `http://127.0.0.1:${PORT}`;
const BETAS = ['managed-agents-2026-04-01'];
const STORE_DIR = `/tmp/awaken-ops-e2e-${process.pid}`;

const req = async (method, path) => {
  const res = await fetch(`${BASE}${path}`, { method });
  return { status: res.status, body: await res.json().catch(() => ({})) };
};

async function main() {
  fs.rmSync(STORE_DIR, { recursive: true, force: true });
  fs.mkdirSync(STORE_DIR, { recursive: true });

  // ---- durable server: the ops verbs operate on a live durable thread ----
  const durable = spawnServer('echo', PORT, { AWAKEN_STORAGE_DIR: STORE_DIR, AWAKEN_INGRESS: 'durable' });
  await waitForPort(PORT);
  try {
    // A normal turn creates the session and its durable dispatch queue.
    const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: BASE });
    const session = await client.beta.sessions.create({ agent: 'assistant', environment_id: 'env_local', betas: BETAS });
    await client.beta.sessions.events.send(session.id, {
      events: [{ type: 'user.message', content: [{ type: 'text', text: 'OPS-SETUP' }] }],
      betas: BETAS,
    });
    const T = session.id;

    // reconcile (ADR-0011): reclaim runnable work; a clean queue reconciles to none.
    const rec = await req('POST', `/v1/durable/threads/${T}/reconcile`);
    assert.equal(rec.status, 200, 'reconcile ok');
    assert.deepEqual(rec.body.recovered, [], 'clean queue reconciles to no runs');
    pass('reconcile (ADR-0011) reclaims runnable work — clean queue → []');

    // reap (ADR-0015): dead-letter crashed runs past their budget; none here.
    const reap = await req('POST', `/v1/durable/threads/${T}/reap?max_attempts=1`);
    assert.equal(reap.status, 200, 'reap ok');
    assert.equal(reap.body.dead_lettered, 0, 'no crashed runs to dead-letter');
    pass('reap (ADR-0015) dead-letters past-budget crashes — none → 0');

    // dead-letters + purge (ADR-0015): observe and GC the dead-letter set.
    const dl = await req('GET', `/v1/durable/threads/${T}/dead-letters`);
    assert.equal(dl.status, 200, 'dead-letters ok');
    assert.deepEqual(dl.body.dead_letters, [], 'no dead letters on a healthy thread');
    const purge = await req('POST', `/v1/durable/threads/${T}/dead-letters/purge`);
    assert.equal(purge.status, 200, 'purge ok');
    assert.equal(purge.body.purged, 0, 'nothing to GC');
    pass('dead-letters + purge (ADR-0015) surface the dead-letter set and GC it');
  } finally {
    await stopServer(durable.server);
  }

  // ---- non-durable server: every ops verb fails closed (400) ----
  const direct = spawnServer('echo', PORT, {});
  await waitForPort(PORT);
  try {
    for (const [method, path] of [
      ['POST', `/v1/durable/threads/any/reconcile`],
      ['POST', `/v1/durable/threads/any/reap`],
      ['GET', `/v1/durable/threads/any/dead-letters`],
      ['POST', `/v1/durable/threads/any/dead-letters/purge`],
    ]) {
      const r = await req(method, path);
      assert.equal(r.status, 400, `${method} ${path} fails closed without durable ingress`);
    }
    pass('every durable ops verb fails closed (400) when AWAKEN_INGRESS is unset');

    console.log('E2E PASS: durable operations surface (ADR-0011 reconcile / ADR-0015 dead-letter GC) via HTTP.');
  } finally {
    await stopServer(direct.server);
    fs.rmSync(STORE_DIR, { recursive: true, force: true });
  }
}

main().catch((err) => {
  console.error('E2E FAIL:', err);
  process.exitCode = 1;
});
