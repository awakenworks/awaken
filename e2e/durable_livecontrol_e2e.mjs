// Durable live-control end-to-end (ADR-0016 cancellation + ADR-0054 wake) via the durable
// operations surface.
//
// A run is background-submitted and the daemon drains it; the probe model's write
// tool awaits, so the run sits queued/awaiting in the dispatch store. We then cancel
// it by id through the live-control seam: the Host first persists the durable intent,
// then nudges an already-owned local attempt while the process pool commits and
// settles the terminal `Cancelled` fact. We also assert that a wake with no live
// subscriber is a hard error (G5) — durable-only operations never silently succeed.
//
// Run: (from e2e/)  node durable_livecontrol_e2e.mjs

import assert from 'node:assert/strict';
import fs from 'node:fs';
import {
  spawnServer,
  stopServer,
  waitForPort,
  waitForValue,
  pass,
  startUpstream,
  realServerEnv,
} from './harness.mjs';

const PORT = Number(process.env.E2E_PORT ?? 38182);
const BASE = `http://127.0.0.1:${PORT}`;
const STORE_DIR = `/tmp/awaken-livectl-e2e-${process.pid}`;
const THREAD = 'durable-livecontrol-e2e';

const post = async (path, body) => {
  const res = await fetch(`${BASE}${path}`, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify(body ?? {}),
  });
  return { status: res.status, body: await res.json().catch(() => ({})) };
};
const committed = (thread) => fetch(`${BASE}/v1/durable/threads/${thread}/messages`).then((r) => r.json());

async function main() {
  fs.rmSync(STORE_DIR, { recursive: true, force: true });
  fs.mkdirSync(STORE_DIR, { recursive: true });
  const upstream = await startUpstream('probe');
  const srv = spawnServer('real', PORT, {
    SESSION_DEPLOYMENT_STORAGE_DIR: STORE_DIR,
    SESSION_DEPLOYMENT_INGRESS: 'durable',
    AWAKEN_DISPATCH_DAEMON: '1',
    ...realServerEnv('probe', upstream),
  });
  await waitForPort(PORT);
  try {
    // Ownership decision row L1: operational live control exercises one ordinary
    // Runtime Thread admitted by generic durable ingress. Managed Session roots
    // are deliberately absent because Session reservation owns their Run ingress.
    const T = THREAD;

    const sub = await post(`/v1/durable/threads/${T}/submit_background`, { text: 'CANCEL-ME' });
    assert.equal(sub.status, 200, 'background submit accepted');
    const runId = sub.body.run_id;
    pass(`run ${runId} background-submitted; daemon awaits it on the write tool`);

    // The queue snapshot must reach the exact approval boundary before this
    // live-control scenario cancels it. Claim/publication races belong to the
    // dispatch fencing tests, not to this HTTP status oracle.
    const awaiting = await waitForValue(
      async () => (await (await fetch(`${BASE}/v1/durable/threads/${T}/dispatches`)).json()).dispatches ?? [],
      (dispatches) => dispatches.find((dispatch) => dispatch.run_id === runId)?.status === 'Awaiting',
      'the exact dispatch to reach Awaiting',
      { timeoutMs: 5_000, pollMs: 50 },
    );
    assert.equal(
      awaiting.find((dispatch) => dispatch.run_id === runId)?.status,
      'Awaiting',
      'the write boundary is durably awaiting approval',
    );
    pass('dispatch queue snapshot surfaces the exact Awaiting run (ADR-0025)');

    const cancellation = await post(`/v1/durable/threads/${T}/cancel`, { run_id: runId });
    assert.equal(cancellation.status, 200, 'the durable cancellation intent is accepted once');
    assert.equal(cancellation.body.cancelled, true, 'the exact run is cancelled');
    assert.equal(cancellation.body.run_id, runId, 'the exact run cancellation was accepted');
    pass('durable cancellation intent accepted through the ADR-0016 control seam');

    // Live-control cause/effect table: C1 the exact dispatch exists; C2 its
    // cancellation intent is accepted at the Awaiting boundary; C3 the ordinary
    // pool settles that dispatch; C4 no exact Runtime subscriber remains.
    // E1 C1+C2+C3 removes the row; E2 C3+C4 maps Wake to NoSubscriber/400
    // without reopening Session/Environment. Rules L1=C1+C2+C3=>E1 and
    // L2=C2+C3+C4=>E2. The deeper cancellation crash/late-completion matrix
    // remains owned by durable_worker_cancel_e2e; this scenario owns the served
    // live-control edge.
    await waitForValue(
      async () => (await (await fetch(`${BASE}/v1/durable/threads/${T}/dispatches`)).json()).dispatches ?? [],
      (dispatches) => !dispatches.some((dispatch) => dispatch.run_id === runId),
      'the cancelled dispatch to settle',
      { timeoutMs: 10_000, pollMs: 25 },
    );

    // It stays cancelled: no `done` reply ever appears (a cancelled run never
    // resumes to completion).
    const { messages } = await committed(T);
    assert.ok(
      !messages.some((m) => m.role === 'Assistant' && m.text.includes('done')),
      'the settled cancellation observation contains no assistant completion',
    );
    pass('the settlement observation contains no assistant completion');

    // Fail-closed: wake with no live subscriber is a hard error, not a silent ok.
    const wake = await post(`/v1/durable/threads/${T}/wake`, { run_id: runId });
    assert.equal(wake.status, 400, 'wake is fail-closed with no live subscriber');
    assert.match(wake.body.error ?? '', /no live subscriber/, 'wake reports the live-control authority');
    pass('wake fails closed with no live subscriber (G5, live-only)');

    console.log('E2E PASS: ADR-0016 durable cancel + ADR-0054 fail-closed wake.');
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
