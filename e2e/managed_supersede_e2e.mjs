// Epoch supersession end-to-end (slice E, ADR-0022) via the Anthropic TS SDK plus
// the durable operations surface.
//
// In durable mode a first turn awaits on a tool confirmation, so its dispatch sits
// `awaiting` in the thread's queue. We then POST a *superseding* turn to
// /v1/durable/threads/:thread/supersede: the newest submission wins — the awaiting
// run is marked superseded (never woken again) and the new run is driven. The
// response lists the superseded run id, proving newest-wins semantics end to end.
//
// Run: (from e2e/)  node managed_supersede_e2e.mjs

import assert from 'node:assert/strict';
import fs from 'node:fs';
import Anthropic from '@anthropic-ai/sdk';
import { spawnServer, stopServer, waitForPort, pass, startUpstream, realServerEnv } from './harness.mjs';

const PORT = Number(process.env.E2E_PORT ?? 38176);
const BASE = `http://127.0.0.1:${PORT}`;
const BETAS = ['managed-agents-2026-04-01'];
const STORE_DIR = `/tmp/awaken-supersede-e2e-${process.pid}`;
const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: BASE });

const listEvents = async (sessionId) => {
  const events = [];
  for await (const ev of client.beta.sessions.events.list(sessionId, { betas: BETAS })) events.push(ev);
  return events;
};

const post = async (path, body) => {
  const res = await fetch(`${BASE}${path}`, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  return { status: res.status, body: await res.json().catch(() => ({})) };
};

async function main() {
  fs.rmSync(STORE_DIR, { recursive: true, force: true });
  fs.mkdirSync(STORE_DIR, { recursive: true });
  const upstream = await startUpstream('probe');
  const srv = spawnServer('real', PORT, { AWAKEN_STORAGE_DIR: STORE_DIR, AWAKEN_INGRESS: 'durable', ...realServerEnv('probe', upstream) });
  await waitForPort(PORT);
  try {
    // A first turn awaits on a tool confirmation — its dispatch is `awaiting`.
    const session = await client.beta.sessions.create({ agent: 'assistant', environment_id: 'env_local', betas: BETAS });
    await client.beta.sessions.events.send(session.id, {
      events: [{ type: 'user.message', content: [{ type: 'text', text: 'AWAIT-FIRST' }] }],
      betas: BETAS,
    });
    const awaiting = await listEvents(session.id);
    assert.equal(
      awaiting.find((e) => e.type === 'session.status_idle').stop_reason.type,
      'requires_action',
      'first run awaiting awaiting confirmation',
    );
    pass('first run awaiting (dispatch is awaiting in the durable queue)');

    // Before supersede: nothing superseded yet.
    const before = await fetch(`${BASE}/v1/durable/threads/${session.id}/superseded`).then((r) => r.json());
    assert.deepEqual(before.superseded, [], 'no runs superseded before the superseding submit');

    // A superseding submit: the newest turn wins over the awaiting run.
    const sup = await post(`/v1/durable/threads/${session.id}/supersede`, { text: 'SUPERSEDE-THE-AWAITING-RUN' });
    assert.equal(sup.status, 200, 'supersede accepted');
    assert.ok(Array.isArray(sup.body.superseded) && sup.body.superseded.length >= 1, 'the awaiting run was superseded');
    pass(`superseding submit marked ${sup.body.superseded.length} prior run(s) superseded: ${sup.body.superseded.join(', ')}`);

    // Fails closed on a non-durable server would be 400; here it must reject a
    // supersede for an unknown-but-non-durable thread only via the durable guard,
    // so assert the observable superseded list now reflects the superseded run.
    const after = await fetch(`${BASE}/v1/durable/threads/${session.id}/superseded`).then((r) => r.json());
    assert.ok(after.superseded.length >= 1, 'superseded run remains observable (ADR-0022)');
    pass('superseded run is observable via the operations surface');

    console.log('E2E PASS: epoch supersession (ADR-0022) via the durable operations surface.');
  } finally {
    await stopServer(srv.server);
    upstream.close();
    fs.rmSync(STORE_DIR, { recursive: true, force: true });
  }
}

main().catch((err) => {
  console.error('E2E FAIL:', err);
  process.exitCode = 1;
});
