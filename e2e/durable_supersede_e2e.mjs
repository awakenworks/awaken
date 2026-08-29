// Epoch supersession end-to-end (slice E, ADR-0022) through the durable
// operations surface.
//
// In durable mode a custom-tool Run awaits on its client result, so its dispatch
// sits `awaiting` in the ordinary Runtime thread's queue. We then POST a
// *superseding* turn to
// /v1/durable/threads/:thread/supersede: the newest submission wins — the awaiting
// run is marked superseded (never woken again) and the new run is driven. The
// response lists the superseded run id, proving newest-wins semantics end to end.
// Managed Session roots are intentionally absent: their Run reservation is owned
// by Session ingress, not this RunDispatch operations API.
//
// Run: (from e2e/)  node durable_supersede_e2e.mjs

import assert from 'node:assert/strict';
import fs from 'node:fs';
import {
  pass,
  spawnServer,
  stopServer,
  waitForValue,
  waitForPort,
} from './harness.mjs';

const PORT = Number(process.env.E2E_PORT ?? 38176);
const BASE = `http://127.0.0.1:${PORT}`;
const STORE_DIR = `/tmp/awaken-supersede-e2e-${process.pid}`;
const THREAD = `durable-supersede-${process.pid}`;

const post = async (path, body) => {
  const res = await fetch(`${BASE}${path}`, {
    method: 'POST',
    headers: body === undefined ? {} : { 'content-type': 'application/json' },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  return { status: res.status, body: await res.json().catch(() => ({})) };
};
const get = async (path) => (await fetch(`${BASE}${path}`)).json();

async function main() {
  fs.rmSync(STORE_DIR, { recursive: true, force: true });
  fs.mkdirSync(STORE_DIR, { recursive: true });
  const srv = spawnServer('custom', PORT, {
    SESSION_DEPLOYMENT_STORAGE_DIR: STORE_DIR,
    SESSION_DEPLOYMENT_INGRESS: 'durable',
  });
  await waitForPort(PORT);
  try {
    // Cause/effect graph: C1 an ordinary durable Thread submits a Run; C2 its
    // custom tool commits Awaiting; C3 supersede is posted on the same Thread.
    // Effects: E1 C2 identifies one stale dispatch; E2 C3 atomically marks E1
    // Superseded and admits the replacement; E3 the query returns that exact id.
    // Decision S1 C1&&!C2=>observe; S2 C1+C2+C3=>E1+E2+E3. Constraint: a
    // Managed Session root is ineligible because Session ingress owns its Run
    // reservation and receipt.
    const submitted = await post(`/v1/durable/threads/${THREAD}/submit_background`, {
      agent: 'assistant',
      text: 'AWAIT-FIRST',
    });
    assert.equal(submitted.status, 200, `initial durable submit: ${JSON.stringify(submitted.body)}`);
    const awaiting = await waitForValue(
      () => get(`/v1/durable/threads/${THREAD}/dispatches`),
      (body) => body.dispatches?.some((row) => row.status === 'Awaiting'),
      'S1 first durable Run to commit its Awaiting dispatch',
    );
    const staleRunId = awaiting.dispatches.find((row) => row.status === 'Awaiting')?.run_id;
    assert.equal(staleRunId, submitted.body.run_id, 'the submitted Run is the awaiting dispatch');
    pass('first run awaiting (dispatch is awaiting in the durable queue)');

    // Before supersede: nothing superseded yet.
    const before = await get(`/v1/durable/threads/${THREAD}/superseded`);
    assert.deepEqual(before.superseded, [], 'no runs superseded before the superseding submit');

    // A superseding submit: the newest turn wins over the awaiting run.
    const sup = await post(`/v1/durable/threads/${THREAD}/supersede`, {
      agent: 'assistant',
      text: 'SUPERSEDE-THE-AWAITING-RUN',
    });
    assert.equal(sup.status, 200, `supersede accepted: ${JSON.stringify(sup.body)}`);
    assert.ok(sup.body.superseded?.includes(staleRunId), 'the exact awaiting run was superseded');
    pass(`superseding submit marked ${sup.body.superseded.length} prior run(s) superseded: ${sup.body.superseded.join(', ')}`);

    // Fails closed on a non-durable server would be 400; here it must reject a
    // supersede for an unknown-but-non-durable thread only via the durable guard,
    // so assert the observable superseded list now reflects the superseded run.
    const after = await get(`/v1/durable/threads/${THREAD}/superseded`);
    assert.ok(after.superseded.length >= 1, 'superseded run remains observable (ADR-0022)');
    pass('superseded run is observable via the operations surface');

    console.log('E2E PASS: epoch supersession (ADR-0022) via the durable operations surface.');
  } finally {
    await stopServer(srv.server);
    fs.rmSync(STORE_DIR, { recursive: true, force: true });
  }
}

main().catch((err) => {
  console.error('E2E FAIL:', err);
  process.exitCode = 1;
});
