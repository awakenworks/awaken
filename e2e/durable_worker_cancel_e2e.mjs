// Durable cancellation through both the operational API and Managed Agents SDK.
//
// The operational rule cancels one Awaiting run. Managed rules crash after a
// pre-dispatch cancellation intent and during an unacknowledged model call.
// The former commits Cancelled; the latter stays Indeterminate and cannot replay.
//
// Run: node e2e/durable_worker_cancel_e2e.mjs

import fs from 'node:fs';
import assert from 'node:assert/strict';
import Anthropic from '@anthropic-ai/sdk';
import {
  spawnServer,
  stopServer,
  hardKillServer as hardKill,
  waitForPort,
  waitForSessionEventReceipt,
  waitForValue,
  pass,
  publishAlwaysAskManagementProbeAgent,
  startUpstream,
  realServerEnv,
} from './harness.mjs';
import {
  sqliteDatabaseForThread,
  sqliteExec,
  sqliteRows,
  sqliteRun,
  sqliteScalar,
} from './sqlite.mjs';

const PORT = Number(process.env.E2E_PORT ?? 39723);
const BASE = `http://127.0.0.1:${PORT}`;
const BETAS = ['managed-agents-2026-04-01'];
const THREAD = 'durable-cancel-1';
const AWAITING_AGENT = 'durable-cancel-awaiting-agent';
const STORE = `/tmp/awaken-durable-cancel-${process.pid}`;
const INTENT_STORE = `${STORE}-intent`;
const UNKNOWN_STORE = `${STORE}-unknown`;
const MARK = 'CANCEL-ME-EFFECT';
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

const post = async (path, body) => {
  const res = await fetch(`${BASE}${path}`, {
    method: 'POST',
    headers: body === undefined ? {} : { 'content-type': 'application/json' },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  return { status: res.status, body: await res.json().catch(() => ({})) };
};
const get = async (path) => {
  const res = await fetch(`${BASE}${path}`);
  return { status: res.status, body: await res.json().catch(() => ({})) };
};

function spawnDurable(store, upstream, behavior, coordinatorOnly = false) {
  return spawnServer('real', PORT, {
    SESSION_DEPLOYMENT_INGRESS: 'durable',
    SESSION_DEPLOYMENT_STORAGE_DIR: store,
    ...(coordinatorOnly ? { SESSION_DEPLOYMENT_DISABLE_LOCAL_POOL: '1' } : {}),
    ...realServerEnv(behavior, upstream),
  });
}

async function managedSession(srv) {
  const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: srv.baseUrl });
  const session = await client.beta.sessions.create({
    agent: 'assistant',
    environment_id: 'env_local',
    betas: BETAS,
  });
  return { client, session };
}

async function submitManagedRun(client, sessionId, text, rule) {
  // Managed-ingress cause/effect row I1: an official SDK User event targets an
  // active Session root. Effect: Session/ThreadCommit reserves exactly one Run
  // and publishes its dispatch; generic durable submit is not a second ingress.
  const submitted = await client.beta.sessions.events.send(sessionId, {
    events: [{ type: 'user.message', content: [{ type: 'text', text }] }],
    betas: BETAS,
  });
  assert.equal(typeof submitted.data[0]?.id, 'string', `${rule} Session event batch accepted`);
  const dispatch = await waitForValue(
    async () => (await get(`/v1/durable/threads/${sessionId}/dispatches`)).body.dispatches?.[0],
    (row) => typeof row?.run_id === 'string',
    `${rule} Session-owned Run dispatch publication`,
    { timeoutMs: 10_000, pollMs: 20 },
  );
  return dispatch.run_id;
}

async function interruptManagedRun(client, sessionId, rule) {
  const receipt = await client.beta.sessions.events.send(sessionId, {
    events: [{ type: 'user.interrupt' }],
    betas: BETAS,
  });
  const receiptId = receipt.data[0]?.id;
  assert.equal(typeof receiptId, 'string', `${rule} SDK interrupt returned an exact receipt`);
  await waitForSessionEventReceipt(
    client,
    sessionId,
    receiptId,
    BETAS,
    () => true,
    `${rule} SDK interrupt receipt to be processed`,
  );
}

function dispatchRow(database, runId) {
  return sqliteRows(
    database,
    `SELECT status, cancel_requested, lease_owner, lease_until, lease_epoch,
            retry_not_before, attempt_count,
            active_attempt_owner, active_attempt_epoch
     FROM runtime_dispatch WHERE run_id = ?`,
    runId,
  )[0] ?? null;
}

const completionCount = (database, runId) => Number(sqliteScalar(
  database,
  'SELECT COUNT(*) FROM runtime_dispatch_completion WHERE run_id = ?',
  runId,
));
const runEndCause = (database, runId) => JSON.parse(sqliteScalar(
  database,
  'SELECT phase FROM runtime_run_record WHERE run_id = ?',
  runId,
)).Ended;

async function waitForRunDatabase(root, threadId, rule) {
  return waitForValue(
    () => {
      try {
        return sqliteDatabaseForThread(root, threadId, 'runtime_message');
      } catch {
        return null;
      }
    },
    (database) => typeof database === 'string',
    `${rule} Session commit database`,
    { timeoutMs: 10_000, pollMs: 25 },
  );
}

async function crashAfterManagedInterruptIntent() {
  fs.rmSync(INTENT_STORE, { recursive: true, force: true });
  fs.mkdirSync(INTENT_STORE, { recursive: true });
  const database = `${INTENT_STORE}/dispatch.db`;
  const upstream = await startUpstream('probe');
  let srv = spawnDurable(INTENT_STORE, upstream, 'probe', true);

  try {
    await waitForPort(PORT, 180_000, srv.server);
    const { client, session } = await managedSession(srv);
    // Managed cancel decision table. C1 durable Coordinator has no local pool;
    // C2 SDK interrupt persists; C3 process dies before a Worker claims; C4
    // replacement commits Cancelled without provider I/O; C5 test trigger
    // rejects only exact Dispatch Done; C6 second crash removes that trigger.
    // E1 pending cancel intent; E2 committed Cancelled plus empty physical slot
    // and retained dispatch; E3 one fenced Done repair/no inference; E4 later
    // re-cancel fails closed. The fixture changes no Run, cancel bit, physical
    // slot or completion fact: Session/Run/Worker own them.
    // | Rule | Causes | Effects |
    // | R1 | C1+C2+C3 | E1 |
    // | R2 | E1+C4+C5 | E2 |
    // | R3 | E2+C6 | E3+E4 |
    const runId = await submitManagedRun(
      client,
      session.id,
      'cancel this queued Managed Session run before any Worker can claim it',
      'R1',
    );
    await interruptManagedRun(client, session.id, 'R1');
    const retained = await waitForValue(
      () => dispatchRow(database, runId),
      (row) => Number(row?.cancel_requested) === 1,
      'R1 durable cancellation intent to be retained before crash',
      { timeoutMs: 10_000, pollMs: 25 },
    );
    assert.deepEqual(
      [retained.status, retained.lease_owner, retained.lease_until,
        Number(retained.attempt_count), completionCount(database, runId), upstream.received],
      ['pending', null, null, 0, 0, 0],
      'R1 has one unclaimed intent and no completion/provider effect',
    );
    pass('R1 retained one unclaimed cancellation intent before the crash');

    await hardKill(srv.server, 'R1');
    srv = null;
    // R2 holds only the exact queue Done mutation. The restarted Worker remains
    // the sole owner of the Cancelled Run commit and has no model to call.
    const runLiteral = sqliteScalar(database, 'SELECT quote(?)', runId);
    assert.equal(typeof runLiteral, 'string', 'R2 SQLite owns exact run-id quoting');
    sqliteExec(database, `CREATE TRIGGER r2_block_dispatch_done
      BEFORE DELETE ON runtime_dispatch WHEN OLD.run_id = ${runLiteral}
      BEGIN SELECT RAISE(ABORT, 'R2 settle crash boundary'); END`);
    srv = spawnDurable(INTENT_STORE, upstream, 'probe');
    await waitForPort(PORT, 180_000, srv.server);
    const runDatabase = await waitForRunDatabase(INTENT_STORE, session.id, 'R2');
    const terminalRetained = await waitForValue(
      () => ({
        cause: runEndCause(runDatabase, runId),
        dispatch: dispatchRow(database, runId),
        completions: completionCount(database, runId),
      }),
      ({ cause, dispatch, completions }) => cause === 'Cancelled'
        && dispatch !== null
        && Number(dispatch.cancel_requested) === 1
        && dispatch.active_attempt_epoch === null
        && completions === 0,
      'R2 Cancelled Run committed while only its exact Done settlement is held',
      { timeoutMs: 20_000, pollMs: 25 },
    );
    assert.ok(['pending', 'running'].includes(terminalRetained.dispatch.status),
      'R2 held terminal dispatch stays repairable');
    assert.equal(upstream.received, 0, 'R2 cancellation never entered the provider');
    pass('R2 retained committed Cancelled with an empty physical slot and no Done receipt');

    await hardKill(srv.server, 'R3');
    srv = null;
    sqliteExec(database, 'DROP TRIGGER r2_block_dispatch_done');
    const retry = dispatchRow(database, runId);
    assert.ok(retry, 'R3 exact terminal row survives the second crash');
    if (retry.status === 'running') {
      const expired = sqliteRun(database,
        "UPDATE runtime_dispatch SET lease_until = 0 WHERE run_id = ? AND status = 'running'",
        runId);
      assert.equal(Number(expired.changes), 1, 'R3 expires only the failed settlement claim');
    }
    // The retry deadline belongs to Dispatch, so the fixture advances only its
    // own observation time, never the retained Run, slot or completion facts.
    const waitMs = Math.max(0, Number(retry.retry_not_before ?? 0) - Date.now());
    if (waitMs > 0) await sleep(waitMs + 1);
    srv = spawnDurable(INTENT_STORE, upstream, 'probe', true);
    await waitForPort(PORT, 180_000, srv.server);
    const reconcile = await post(`/v1/durable/threads/${session.id}/reconcile`, undefined);
    assert.equal(reconcile.status, 200, `R3 reconcile accepted: ${JSON.stringify(reconcile.body)}`);
    assert.ok(JSON.stringify(reconcile.body.recovered) === '[]'
      || JSON.stringify(reconcile.body.recovered) === JSON.stringify([runId]),
    'R3 only startup or explicit reconciliation owns the exact terminal claim');
    await waitForValue(
      () => [dispatchRow(database, runId), completionCount(database, runId)],
      ([row, completions]) => row === null && completions === 1,
      'R3 committed terminal repair to settle the retained cancellation once',
      { timeoutMs: 20_000, pollMs: 25 },
    );
    assert.equal(runEndCause(runDatabase, runId), 'Cancelled', 'R3 preserves the committed cause');
    assert.equal(upstream.received, 0, 'R3 recovery performs no inference');
    const again = await post(`/v1/durable/threads/${session.id}/cancel`, { run_id: runId });
    assert.equal(again.status, 400, 'R3 re-cancel after terminal settlement fails closed');
    pass('R3 repaired the same committed cancellation without Provider inference');
  } finally {
    if (srv) await stopServer(srv.server);
    await upstream.close();
    fs.rmSync(INTENT_STORE, { recursive: true, force: true });
  }
}

async function inFlightCancellationRetainsUnknown() {
  fs.rmSync(UNKNOWN_STORE, { recursive: true, force: true });
  fs.mkdirSync(UNKNOWN_STORE, { recursive: true });
  const database = `${UNKNOWN_STORE}/dispatch.db`;
  const upstream = await startUpstream('echo', { firstDelayMs: 30_000 });
  let srv = spawnDurable(UNKNOWN_STORE, upstream, 'echo');

  try {
    await waitForPort(PORT, 180_000, srv.server);
    const { client, session } = await managedSession(srv);
    // In-flight cancel decision table: C1 an exact Worker/model request is
    // running; C2 the SDK interrupt is durably accepted before a complete
    // response; C3 the process dies; C4 the lease expires. C1+C2 commits
    // Indeterminate with an occupied physical slot and no Done receipt;
    // C1+C2+C3+C4 may reclaim a mutation epoch but cannot replay the model.
    // | U1 | C1+C2 | Indeterminate, occupied slot, no Done |
    // | U2 | U1+C3+C4 | higher mutation epoch, no replay |
    // The unclaimed-intent case above owns the positive Cancelled partition.
    const runId = await submitManagedRun(
      client,
      session.id,
      'hold this Managed Session run in inference until it is interrupted',
      'U1',
    );
    await waitForValue(
      () => upstream.received,
      (received) => received === 1,
      'U1 canonical Worker to enter Provider inference',
      { timeoutMs: 15_000, pollMs: 20 },
    );

    const runDatabase = await waitForRunDatabase(UNKNOWN_STORE, session.id, 'U1');
    const running = await waitForValue(
      () => dispatchRow(database, runId),
      (row) => row?.status === 'running' && row.lease_owner !== null,
      'U1 exact dispatch to be leased by the canonical Worker',
      { timeoutMs: 10_000, pollMs: 20 },
    );
    assert.deepEqual(
      [Number(running.cancel_requested), Number(running.attempt_count)],
      [0, 0],
      'U1 running claim starts uncancelled and unretried',
    );

    await interruptManagedRun(client, session.id, 'U1');
    const unknownRetained = await waitForValue(
      () => ({
        cause: runEndCause(runDatabase, runId),
        dispatch: dispatchRow(database, runId),
        completions: completionCount(database, runId),
      }),
      ({ cause, dispatch, completions }) => cause === 'Indeterminate'
        && dispatch?.status === 'running'
        && Number(dispatch.cancel_requested) === 1
        && dispatch.active_attempt_owner !== null
        && dispatch.active_attempt_epoch !== null
        && completions === 0,
      'U1 committed Indeterminate after the interrupted model effect stayed unknown',
      { timeoutMs: 45_000, pollMs: 25 },
    );
    assert.ok(unknownRetained.dispatch.lease_owner && Number(unknownRetained.dispatch.lease_until) > 0,
      'U1 retains a current mutation claim');
    assert.ok(unknownRetained.dispatch.active_attempt_epoch > 0
      && unknownRetained.dispatch.active_attempt_epoch <= unknownRetained.dispatch.lease_epoch,
    'U1 retains the occupied predecessor physical slot across any newer claim');
    assert.equal(upstream.received, 1, 'U1 no second provider request');
    pass('U1 retained committed Indeterminate and the exact occupied attempt');

    await hardKill(srv.server, 'U2');
    srv = null;
    const providerRequestsBeforeRestart = upstream.received;
    const accelerated = sqliteRun(
      database,
      "UPDATE runtime_dispatch SET lease_until = 0 WHERE run_id = ? AND status = 'running'",
      runId,
    );
    assert.equal(Number(accelerated.changes), 1, 'U2 expires only the exact abandoned claim');
    srv = spawnDurable(UNKNOWN_STORE, upstream, 'echo');
    await waitForPort(PORT, 180_000, srv.server);
    const reclaimed = await waitForValue(
      () => dispatchRow(database, runId),
      (row) => row?.lease_epoch > unknownRetained.dispatch.lease_epoch
        && row.active_attempt_epoch === unknownRetained.dispatch.active_attempt_epoch,
      'U2 replacement to retain the old physical slot under a higher mutation epoch',
      { timeoutMs: 10_000, pollMs: 20 },
    );
    assert.deepEqual(
      [runEndCause(runDatabase, runId), Number(reclaimed.cancel_requested),
        completionCount(database, runId), upstream.received],
      ['Indeterminate', 1, 0, providerRequestsBeforeRestart],
      'U2 retains committed uncertainty and never replays the model',
    );
    pass('U2 reclaimed the logical lease but retained the unknown physical effect');
  } finally {
    if (srv) await stopServer(srv.server);
    await upstream.close();
    fs.rmSync(UNKNOWN_STORE, { recursive: true, force: true });
  }
}

async function cancelAwaitingDurableRun() {
  fs.rmSync(STORE, { recursive: true, force: true });
  fs.mkdirSync(STORE, { recursive: true });
  const srv = spawnServer('management-probe', PORT, {
    SESSION_DEPLOYMENT_INGRESS: 'durable',
    SESSION_DEPLOYMENT_STORAGE_DIR: STORE,
  });
  await waitForPort(PORT);
  try {
    await publishAlwaysAskManagementProbeAgent(BASE, AWAITING_AGENT);
    // Awaiting-cancel cause/effect graph:
    // C1=a canonical Worker has committed one Awaiting approval boundary;
    // C2=the durable operations API cancels its exact run id; C3=reconcile and
    // elapsed time follow cancellation; C4=the run id is unknown or already Done.
    // Effects: E1=C1+C2 commits Cancelled and removes the dispatch; E2=C3 never
    // re-drives or double-commits the run; E3=C4 fails closed with HTTP 400.
    // Constraint: the never-approved tool effect must remain absent; queue and
    // committed Thread truth are observed only through production APIs.
    //
    // | Rule | Awaiting | exact id | later reconcile | id live | Expected |
    // |---|---|---|---|---|---|
    // | R0a | T | T | T | T | E1 + E2 |
    // | R0b | any | any | any | F | E3 |
    // A background run awaits on the write tool — the probe model asks for approval,
    // so submit_background returns after the run awaiting (never resumed).
    const submit = await post(`/v1/durable/threads/${THREAD}/submit_background`, {
      agent: AWAITING_AGENT,
      text: MARK,
    });
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
    assert.equal(cancel.status, 200, `cancel accepted: ${JSON.stringify(cancel.body)}`);
    assert.equal(cancel.body.cancelled, true, 'cancel reported the run cancelled');
    pass('cancelled the awaiting durable run by id');

    // The API acknowledges the durable cancellation intent. A process-pool worker
    // may already own its claim, so terminal settlement is asynchronously observable
    // even though the intent itself was accepted synchronously.
    let afterRows = [];
    for (let i = 0; i < 200; i++) {
      afterRows = (await get(`/v1/durable/threads/${THREAD}/dispatches`)).body.dispatches;
      if (!afterRows.some((d) => d.run_id === runId)) break;
      await sleep(25);
    }
    assert.ok(!afterRows.some((d) => d.run_id === runId), 'the cancelled run is removed from the dispatch queue');
    pass('cancelled run removed from the dispatch queue (not runnable)');

    // The exact cancellation is terminal: reconciliation cannot re-drive it,
    // and the committed transcript contains no tool effect. The empty queue
    // and no-recovery receipt are durable facts; a fixed delay adds no proof.
    const rec = await post(`/v1/durable/threads/${THREAD}/reconcile`, undefined);
    assert.equal(rec.status, 200, 'reconcile ok');
    assert.ok(!(rec.body.recovered ?? []).includes(runId), 'reconcile does not re-drive the cancelled run');

    const committed = (await get(`/v1/durable/threads/${THREAD}/messages`)).body.messages;
    assert.ok(!JSON.stringify(committed).includes('done'),
      'the cancelled run committed no terminal reply or tool effect');
    pass('cancelled run remains absent after explicit reconciliation');

    // Fail-closed: an unknown run id, and re-cancelling the now-gone run, both 400.
    const unknown = await post(`/v1/durable/threads/${THREAD}/cancel`, { run_id: 'run-does-not-exist' });
    assert.equal(unknown.status, 400, 'cancel of an unknown run id fails closed (400)');
    const again = await post(`/v1/durable/threads/${THREAD}/cancel`, { run_id: runId });
    assert.equal(again.status, 400, 're-cancel of the already-cancelled run fails closed (400)');
    pass('cancel fails closed (400) for unknown and already-cancelled run ids');

  } finally {
    await stopServer(srv.server);
    fs.rmSync(STORE, { recursive: true, force: true });
  }
}

async function main() {
  try {
    await crashAfterManagedInterruptIntent();
    await inFlightCancellationRetainsUnknown();
    await cancelAwaitingDurableRun();
    console.log('E2E PASS: pre-dispatch Managed cancel repairs one committed Cancelled after two crashes; in-flight cancel retains Indeterminate and never replays; Awaiting cancel settles once.');
  } catch (err) {
    console.error('E2E FAIL:', err);
    process.exitCode = 1;
  }
}

main();
