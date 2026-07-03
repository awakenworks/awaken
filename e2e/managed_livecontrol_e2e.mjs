// Durable live-control end-to-end (slice E follow-up, ADR-0018) via the durable
// operations surface.
//
// A run is background-submitted and the daemon drains it; the probe model's write
// tool parks, so the run sits queued/parked in the dispatch store. We then cancel
// it by id through the live-control seam: the runtime live channel has no in-flight
// run, so it falls through to a DURABLE cancel of the queued/parked dispatch and
// commits a terminal `Cancelled` fact. We also assert the seam is FAIL-CLOSED: an
// unknown run id cannot be cancelled, and a wake with no live subscriber is a hard
// error (G5) — durable-only operations never silently succeed.
//
// Run: (from e2e/)  node managed_livecontrol_e2e.mjs

import assert from 'node:assert/strict';
import fs from 'node:fs';
import Anthropic from '@anthropic-ai/sdk';
import { spawnServer, stopServer, waitForPort, pass } from './harness.mjs';

const PORT = Number(process.env.E2E_PORT ?? 38182);
const BASE = `http://127.0.0.1:${PORT}`;
const BETAS = ['managed-agents-2026-04-01'];
const STORE_DIR = `/tmp/awaken-livectl-e2e-${process.pid}`;
const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: BASE });

const post = async (path, body) => {
  const res = await fetch(`${BASE}${path}`, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify(body ?? {}),
  });
  return { status: res.status, body: await res.json().catch(() => ({})) };
};
const committed = (thread) => fetch(`${BASE}/v1/durable/threads/${thread}/messages`).then((r) => r.json());
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

async function main() {
  fs.rmSync(STORE_DIR, { recursive: true, force: true });
  fs.mkdirSync(STORE_DIR, { recursive: true });
  const srv = spawnServer('probe', PORT, {
    AWAKEN_STORAGE_DIR: STORE_DIR,
    AWAKEN_INGRESS: 'durable',
    AWAKEN_DISPATCH_DAEMON: '1',
  });
  await waitForPort(PORT);
  try {
    const session = await client.beta.sessions.create({ agent: 'assistant', environment_id: 'env_local', betas: BETAS });
    const T = session.id;

    const sub = await post(`/v1/durable/threads/${T}/submit_background`, { text: 'CANCEL-ME' });
    assert.equal(sub.status, 200, 'background submit accepted');
    const runId = sub.body.run_id;
    pass(`run ${runId} background-submitted; daemon parks it on the write tool`);

    // Give the daemon a moment to claim + park the run, then cancel it by id.
    let cancelled = false;
    for (let i = 0; i < 100; i++) {
      const res = await post(`/v1/durable/threads/${T}/cancel`, { run_id: runId });
      if (res.status === 200 && res.body.cancelled) {
        cancelled = true;
        break;
      }
      await sleep(50);
    }
    assert.ok(cancelled, 'the queued/parked run was cancelled via the durable live-control seam');
    pass('durable cancel of a queued/parked run committed a terminal Cancelled fact (ADR-0018)');

    // It stays cancelled: no `done` reply ever appears (a cancelled run never
    // resumes to completion).
    await sleep(300);
    const { messages } = await committed(T);
    assert.ok(
      !messages.some((m) => m.role === 'Assistant' && m.text.includes('done')),
      'the cancelled run never completed',
    );
    pass('the cancelled run did not resume to completion');

    // Fail-closed: an unknown run id cannot be cancelled.
    const ghost = await post(`/v1/durable/threads/${T}/cancel`, { run_id: 'ghost-run-id' });
    assert.equal(ghost.status, 400, 'cancel is fail-closed for an unknown run id');
    pass('cancel fails closed for an unknown run id (G5)');

    // Fail-closed: wake with no live subscriber is a hard error, not a silent ok.
    const wake = await post(`/v1/durable/threads/${T}/wake`, { run_id: runId });
    assert.equal(wake.status, 400, 'wake is fail-closed with no live subscriber');
    pass('wake fails closed with no live subscriber (G5, live-only)');

    console.log('E2E PASS: durable live-control cancel + fail-closed wake (ADR-0018).');
  } finally {
    await stopServer(srv.server);
    fs.rmSync(STORE_DIR, { recursive: true, force: true });
  }
}

main().catch((err) => {
  console.error('E2E FAIL:', err);
  process.exitCode = 1;
});
