// Cancel a durable run mid-flight, over the durable operations surface.
//
// Under AWAKEN_INGRESS=durable a background-submitted run (probe model) awaits on a
// tool awaiting approval — it sits `Awaiting` in the thread's dispatch queue, never
// yet resumed by the worker. We cancel it by run id via
// POST /v1/durable/threads/:thread/cancel: the dispatch is removed and a terminal
// `Cancelled` fact is committed. We then assert the run does NOT later resume and
// does NOT double-commit — its queue row is gone and committed truth is stable and
// never gains the (never-approved) tool effect. Cancelling an unknown run, and
// re-cancelling the same run, fail closed (400) rather than silently succeeding.
//
// Run: node e2e/durable_worker_cancel_e2e.mjs

import fs from 'node:fs';
import assert from 'node:assert/strict';
import { spawnServer, stopServer, waitForPort, pass, startUpstream, realServerEnv } from './harness.mjs';

const PORT = Number(process.env.E2E_PORT ?? 39723);
const BASE = `http://127.0.0.1:${PORT}`;
const THREAD = 'durable-cancel-1';
const STORE = `/tmp/awaken-durable-cancel-${process.pid}`;
const MARK = 'CANCEL-ME-EFFECT';
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

const post = async (path, body) => {
  const res = await fetch(`${BASE}${path}`, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  return { status: res.status, body: await res.json().catch(() => ({})) };
};
const get = async (path) => {
  const res = await fetch(`${BASE}${path}`);
  return { status: res.status, body: await res.json().catch(() => ({})) };
};

async function main() {
  fs.rmSync(STORE, { recursive: true, force: true });
  fs.mkdirSync(STORE, { recursive: true });
  const upstream = await startUpstream('probe');
  const srv = spawnServer('real', PORT, {
    AWAKEN_INGRESS: 'durable',
    AWAKEN_STORAGE_DIR: STORE,
    ...realServerEnv('probe', upstream),
  });
  await waitForPort(PORT);
  try {
    // A background run awaits on the write tool — the probe model asks for approval,
    // so submit_background returns after the run awaiting (never resumed).
    const submit = await post(`/v1/durable/threads/${THREAD}/submit_background`, { text: MARK });
    assert.equal(submit.status, 200, 'submit_background accepted');
    const runId = submit.body.run_id;
    assert.ok(runId && submit.body.queued === true, `queued a durable run (${runId})`);
    pass(`durable run submitted and awaiting (${runId})`);

    // The pool drives it in the background; with the probe model it awaits on the
    // write tool awaiting approval. Poll until it is `Awaiting` in the queue.
    let row = null;
    for (let i = 0; i < 200; i++) {
      row = (await get(`/v1/durable/threads/${THREAD}/dispatches`)).body.dispatches.find((d) => d.run_id === runId);
      if (row && row.status === 'Awaiting') break;
      await sleep(50);
    }
    assert.ok(row, 'the submitted run has a dispatch row');
    assert.equal(row.status, 'Awaiting', 'the run is Awaiting in the durable queue (awaiting approval)');
    pass('run is Awaiting mid-flight in the dispatch queue');

    // Committed truth so far — the (never-approved) tool effect is absent.
    const beforeCancel = (await get(`/v1/durable/threads/${THREAD}/messages`)).body.messages;
    assert.ok(
      !JSON.stringify(beforeCancel).includes('done'),
      'no terminal reply committed before cancel (run is still awaiting)',
    );

    // Cancel the awaiting run by id.
    const cancel = await post(`/v1/durable/threads/${THREAD}/cancel`, { run_id: runId });
    assert.equal(cancel.status, 200, 'cancel accepted');
    assert.equal(cancel.body.cancelled, true, 'cancel reported the run cancelled');
    pass('cancelled the awaiting durable run by id');

    // The dispatch row is gone — the run is no longer claimable/runnable.
    const afterRows = (await get(`/v1/durable/threads/${THREAD}/dispatches`)).body.dispatches;
    assert.ok(!afterRows.some((d) => d.run_id === runId), 'the cancelled run is removed from the dispatch queue');
    pass('cancelled run removed from the dispatch queue (not runnable)');

    // It must NOT later resume or double-commit: reconcile must not re-drive it, and
    // committed truth stays stable and never gains the tool effect over time.
    const rec = await post(`/v1/durable/threads/${THREAD}/reconcile`, undefined);
    assert.equal(rec.status, 200, 'reconcile ok');
    assert.ok(!(rec.body.recovered ?? []).includes(runId), 'reconcile does not re-drive the cancelled run');

    const snap1 = (await get(`/v1/durable/threads/${THREAD}/messages`)).body.messages;
    await sleep(1500);
    const snap2 = (await get(`/v1/durable/threads/${THREAD}/messages`)).body.messages;
    assert.deepEqual(snap2, snap1, 'committed truth is stable after cancel (no late resume, no double-commit)');
    assert.ok(!JSON.stringify(snap2).includes('done'), 'the cancelled run never produced a terminal reply');
    pass('cancelled run never resumed and never double-committed (stable committed truth)');

    // Fail-closed: an unknown run id, and re-cancelling the now-gone run, both 400.
    const unknown = await post(`/v1/durable/threads/${THREAD}/cancel`, { run_id: 'run-does-not-exist' });
    assert.equal(unknown.status, 400, 'cancel of an unknown run id fails closed (400)');
    const again = await post(`/v1/durable/threads/${THREAD}/cancel`, { run_id: runId });
    assert.equal(again.status, 400, 're-cancel of the already-cancelled run fails closed (400)');
    pass('cancel fails closed (400) for unknown and already-cancelled run ids');

    console.log('E2E PASS: cancel a durable run mid-flight — removed from the queue, never resumes, no double-commit.');
    process.exitCode = 0;
  } catch (err) {
    console.error('E2E FAIL:', err);
    process.exitCode = 1;
  } finally {
    await stopServer(srv.server);
    upstream.close();
    fs.rmSync(STORE, { recursive: true, force: true });
  }
}

main();
