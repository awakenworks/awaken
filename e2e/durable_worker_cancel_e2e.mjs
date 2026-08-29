// Durable cancellation through both the operational API and Managed Agents SDK.
//
// The operational rule cancels one Awaiting run. Managed rules crash after the
// cancellation intent and after the terminal commit respectively, then recover
// through the same dispatch/Run/Worker authorities without Provider inference.
//
// Run: node e2e/durable_worker_cancel_e2e.mjs

import fs from 'node:fs';
import assert from 'node:assert/strict';
import Anthropic from '@anthropic-ai/sdk';
import {
  spawnServer,
  stopServer,
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
const TERMINAL_STORE = `${STORE}-terminal`;
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

async function hardKill(server, rule) {
  const exited = new Promise((resolve, reject) => {
    server.once('exit', resolve);
    server.once('error', reject);
  });
  assert.equal(server.kill('SIGKILL'), true, `${rule} hard crash signal reached the coordinator`);
  await exited;
  await stopServer(server);
}

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
    `SELECT status, cancel_requested, lease_owner, lease_until, attempt_count
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
  let upstreamOpen = true;
  let srv = spawnDurable(INTENT_STORE, upstream, 'probe', true);

  try {
    await waitForPort(PORT, 180_000, srv.server);
    const { client, session } = await managedSession(srv);
    // Managed cancel crash-recovery cause/effect graph:
    // C1=an official-SDK Session owns exactly one canonical root dispatch;
    // C2=the coordinator is durable and has no local claim pool; C3=the SDK
    // user.interrupt is processed; C4=the process crashes after cancellation
    // intent persistence but before any Worker terminal/settlement; C5=the same
    // store restarts with its canonical local Worker while the Provider is down.
    // Effects: E1=interrupt retains exactly one pending row with cancel_requested
    // and no lease/completion; E2=restart consumes that intent without inference,
    // commits Cancelled, settles Done exactly once, and removes the dispatch;
    // E3=re-cancel fails closed after the completion tombstone is authoritative.
    // Constraints: the fixture may only hard-stop the real server and deny the
    // Provider after the durable bit is observed; it never writes cancellation,
    // Run terminal, or completion facts. Session/Run/Worker remain the production
    // authorities, and the operational endpoint is used only to inspect/re-cancel.
    //
    // | Rule | C1 | C2 | C3 | C4 | C5 | Expected |
    // |---|---|---|---|---|---|---|
    // | R1 | T | T | T | T | T | E1 + E2 + E3 |
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
    const providerRequestsBeforeRestart = upstream.received;
    await upstream.close();
    upstreamOpen = false;
    srv = spawnDurable(INTENT_STORE, upstream, 'probe');
    await waitForPort(PORT, 180_000, srv.server);
    await waitForValue(
      () => [dispatchRow(database, runId), completionCount(database, runId)],
      ([row, completions]) => row === null && completions === 1,
      'R1 restarted Worker to settle the retained cancellation',
      { timeoutMs: 20_000, pollMs: 25 },
    );
    assert.equal(upstream.received, providerRequestsBeforeRestart, 'R1 recovery performs no inference');
    const again = await post(`/v1/durable/threads/${session.id}/cancel`, { run_id: runId });
    assert.equal(again.status, 400, 'R1 re-cancel after terminal settlement fails closed');
    pass('R1 recovered the cancellation exactly once without Provider inference');
  } finally {
    if (srv) await stopServer(srv.server);
    if (upstreamOpen) await upstream.close();
    fs.rmSync(INTENT_STORE, { recursive: true, force: true });
  }
}

async function recoverCommittedCancellationAfterSettleCrash() {
  fs.rmSync(TERMINAL_STORE, { recursive: true, force: true });
  fs.mkdirSync(TERMINAL_STORE, { recursive: true });
  const database = `${TERMINAL_STORE}/dispatch.db`;
  const upstream = await startUpstream('echo', { firstDelayMs: 30_000 });
  let upstreamOpen = true;
  let srv = spawnDurable(TERMINAL_STORE, upstream, 'echo');

  try {
    await waitForPort(PORT, 180_000, srv.server);
    const { client, session } = await managedSession(srv);
    // Committed-terminal settlement recovery cause/effect graph:
    // C1=the canonical local Worker owns a running Managed Session dispatch;
    // C2=an official-SDK interrupt persists cancellation and cancels that attempt;
    // C3=the real Done settlement alone is faulted after Runtime commits Cancelled;
    // C4=the process crashes with one terminal-but-unsettled leased row; C5=the
    // settlement fault is removed and only that lease is expired; C6=a durable,
    // coordinator-only restart explicitly reconciles the same store.
    // Effects: E1=C1+C2+C3 retains one cancel_requested running row, exact
    // Ended(Cancelled) truth, and no completion tombstone; E2=C4+C5+C6 claims the
    // terminal row without re-execution, performs ordinary fenced Done settlement,
    // writes one completion tombstone, and removes the row; E3=no Provider request
    // occurs during recovery.
    // Constraints: the fixture blocks only DELETE of this run's real dispatch row,
    // then removes that blocker and changes lease_until only. It never writes the
    // cancel bit, Run phase, completion, Session event, or any terminal fact.
    //
    // | Rule | running | SDK interrupt | settle fault | crash | lease expired | reconcile | Expected |
    // |---|---|---|---|---|---|---|---|
    // | R2 | T | T | T | F | F | F | E1 |
    // | R3 | T | T | removed | T | T | T | E2 + E3 |
    const runId = await submitManagedRun(
      client,
      session.id,
      'hold this Managed Session run in inference until it is interrupted',
      'R2',
    );
    await waitForValue(
      () => upstream.received,
      (received) => received === 1,
      'R2 canonical Worker to enter Provider inference',
      { timeoutMs: 15_000, pollMs: 20 },
    );

    const runDatabase = await waitForRunDatabase(TERMINAL_STORE, session.id, 'R2');
    const running = await waitForValue(
      () => dispatchRow(database, runId),
      (row) => row?.status === 'running' && row.lease_owner !== null,
      'R2 exact dispatch to be leased by the canonical Worker',
      { timeoutMs: 10_000, pollMs: 20 },
    );
    assert.deepEqual(
      [Number(running.cancel_requested), Number(running.attempt_count)],
      [0, 0],
      'R2 running claim starts uncancelled and unretried',
    );

    const runLiteral = sqliteScalar(database, 'SELECT quote(?)', runId);
    assert.equal(typeof runLiteral, 'string', 'R2 SQLite owns exact run-id quoting');
    sqliteExec(
      database,
      `CREATE TRIGGER r2_block_dispatch_done
       BEFORE DELETE ON runtime_dispatch
       WHEN OLD.run_id = ${runLiteral}
       BEGIN
         SELECT RAISE(ABORT, 'R2 settle crash boundary');
       END`,
    );

    await interruptManagedRun(client, session.id, 'R2');
    const terminalRetained = await waitForValue(
      () => ({
        cause: runEndCause(runDatabase, runId),
        dispatch: dispatchRow(database, runId),
        completions: completionCount(database, runId),
      }),
      ({ cause, dispatch, completions }) => cause === 'Cancelled'
        && dispatch?.status === 'running'
        && Number(dispatch.cancel_requested) === 1
        && completions === 0,
      'R2 real Cancelled commit to stop at the faulted Done settlement',
      { timeoutMs: 20_000, pollMs: 25 },
    );
    assert.deepEqual(
      [Boolean(terminalRetained.dispatch.lease_owner), Number(terminalRetained.dispatch.lease_until) > 0, upstream.received],
      [true, true, 1],
      'R2 retains one leased terminal row without replacement inference',
    );
    pass('R2 retained one committed Cancelled run at the real Done-settlement boundary');

    await hardKill(srv.server, 'R3');
    srv = null;
    const providerRequestsBeforeRestart = upstream.received;
    await upstream.close();
    upstreamOpen = false;

    // Offline repair removes only the E2E fault. The authoritative Cancelled fact
    // and cancellation bit remain untouched for the restarted Worker to observe.
    sqliteExec(database, 'DROP TRIGGER r2_block_dispatch_done');
    assert.equal(runEndCause(runDatabase, runId), 'Cancelled', 'R3 preserves terminal truth offline');

    srv = spawnDurable(TERMINAL_STORE, upstream, 'echo', true);
    await waitForPort(PORT, 180_000, srv.server);
    assert.equal(
      dispatchRow(database, runId)?.status,
      'running',
      'R3 unexpired terminal lease survives startup maintenance until the explicit recovery step',
    );
    const accelerated = sqliteRun(
      database,
      "UPDATE runtime_dispatch SET lease_until = 0 WHERE run_id = ? AND status = 'running'",
      runId,
    );
    assert.equal(Number(accelerated.changes), 1, 'R3 fixture expires only the retained terminal lease');

    const reconcile = await post(`/v1/durable/threads/${session.id}/reconcile`, undefined);
    assert.equal(reconcile.status, 200, `R3 reconcile accepted: ${JSON.stringify(reconcile.body)}`);
    assert.deepEqual(
      reconcile.body.recovered,
      [runId],
      'R3 explicit reconciliation owns the exact terminal recovery claim',
    );
    await waitForValue(
      () => [dispatchRow(database, runId), completionCount(database, runId)],
      ([row, completions]) => row === null && completions === 1,
      'R3 terminal recovery to remove the settled dispatch',
      { timeoutMs: 10_000, pollMs: 20 },
    );
    assert.deepEqual(
      [runEndCause(runDatabase, runId), upstream.received],
      ['Cancelled', providerRequestsBeforeRestart],
      'R3 preserves terminal cause and performs no inference',
    );
    pass('R3 reconciled the committed cancellation once without re-execution');
  } finally {
    if (srv) await stopServer(srv.server);
    if (upstreamOpen) await upstream.close();
    fs.rmSync(TERMINAL_STORE, { recursive: true, force: true });
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

  } finally {
    await stopServer(srv.server);
    fs.rmSync(STORE, { recursive: true, force: true });
  }
}

async function main() {
  try {
    await crashAfterManagedInterruptIntent();
    await recoverCommittedCancellationAfterSettleCrash();
    await cancelAwaitingDurableRun();
    console.log('E2E PASS: Managed interrupt survives crash; durable cancel removes a run and never double-commits.');
  } catch (err) {
    console.error('E2E FAIL:', err);
    process.exitCode = 1;
  }
}

main();
