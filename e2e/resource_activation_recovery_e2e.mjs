// Cause-effect E2E for durable Session resource activation recovery. The test
// stops the production `awaken` process at the two persisted crash windows and
// proves startup reconciliation converges without an IAM/policy dependency.

import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import {
  cleanupFixtureTree,
  managedWorkspaceClient,
  spawnProduction,
  stopServer as stop,
  hardKillServer,
  waitForPort,
  waitForSessionEventReceipt,
  waitForValue,
} from './harness.mjs';
import { startFakeAnthropic } from './fixtures/fake_anthropic_fixture.mjs';
import { stableFingerprintFixture } from './fixtures/stable_fingerprint_fixture.mjs';
import {
  parseDurableJson,
  sqliteExec,
  sqliteRows,
  sqliteRun,
  stringifyDurableJson,
} from './sqlite.mjs';

const PORT = Number(process.env.E2E_PORT ?? 38436);
const WORKSPACE = `activation-recovery-${process.pid}`;
const AGENT = 'activation-recovery-agent';
const MODEL = 'activation-recovery-model';
const FAKE_KEY = 'sk-activation-recovery-fake'; // awaken-allow(secret):
const MANAGED_BETA = 'managed-agents-2026-04-01';
const MEMORY_BETA = 'agent-memory-2026-07-22';
const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

function start(directory) {
  return spawnProduction(directory, PORT, {
    workspace: WORKSPACE,
    controlSealKey: '00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff',
    // This scenario validates the resource activation state machine, not the
    // host's bwrap/Seatbelt availability. Select the explicit local provider in
    // the fixture's typed deployment instead of inheriting production's
    // fail-closed namespace default.
    fields: { sandbox_tier: 'local' },
  });
}

async function ready() {
  await waitForPort(PORT, 60_000);
}

const scoped = (suffix) =>
  `http://127.0.0.1:${PORT}/v1/workspaces/${WORKSPACE}/${suffix}`;

async function internalJson(method, url, body) {
  // Config publication is an internal seam absent from the Managed SDK.
  // Compatible Session/Memory operations in this scenario must go through
  // `managedWorkspaceClient`.
  const response = await fetch(url, {
    method,
    headers: {
      ...(body === undefined ? {} : { 'content-type': 'application/json' }),
    },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  const text = await response.text();
  return { status: response.status, body: text ? JSON.parse(text) : null };
}

async function authorModel(upstream) {
  const provider = await internalJson('POST', scoped('config/provider-connections'), {
    idempotency_key: 'activation-recovery-provider',
    workspace_id: WORKSPACE,
    provider_id: 'anthropic',
    display_name: 'Anthropic',
    dialect: 'anthropic_messages',
    base_url: `${upstream.url}/v1/`,
    timeout_secs: 30,
    secret: FAKE_KEY,
  });
  assert.equal(provider.status, 201, JSON.stringify(provider.body));
  const agent = await internalJson('PUT', scoped(`config/agents/${AGENT}`), {
    name: AGENT, model: { id: MODEL }, system: 'resource recovery', max_steps: 2,
  });
  assert.equal(agent.status, 200, JSON.stringify(agent.body));
  const publication = await internalJson('POST', scoped(`config/agents/${AGENT}/publish`));
  assert.equal(publication.status, 200, JSON.stringify(publication.body));
}

async function driveSession(client, sessionId, text, { timeoutMs = 45_000 } = {}) {
  // Demand-driver cause/effect table: C1=official SDK send returns one exact
  // User receipt; C2=the registered Worker realizes the frozen generation and
  // processes C1; C3=a later idle edge commits. E1=return only after C1+C2+C3.
  // K: no status poll or second command may stand in for this receipt. Rules:
  // D1 !C1=>fail; D2 C1+(!C2||!C3)=>bounded retry; D3 C1+C2+C3=>E1.
  const response = await client.beta.sessions.events.send(sessionId, {
    events: [{ type: 'user.message', content: [{ type: 'text', text }] }],
    betas: [MANAGED_BETA],
  });
  assert.equal(response.data?.length, 1, JSON.stringify(response));
  const receiptId = response.data[0]?.id;
  assert.equal(typeof receiptId, 'string', 'exact activation User Event receipt');
  return waitForSessionEventReceipt(
    client,
    sessionId,
    receiptId,
    [MANAGED_BETA],
    ({ delta }) => delta.some((event) => event.type === 'session.status_idle'),
    `Resource activation Run ${JSON.stringify(text)} to settle`,
    { timeoutMs },
  );
}

function sqlQuote(value) {
  return `'${String(value).replaceAll("'", "''")}'`;
}

function terminalCleanupEffectId(sessionId) {
  // Exact test-fixture encoding of the contract's stable cleanup identity. The
  // production aggregate remains the authority; this helper only lets the SQL
  // crash seam represent the post-Requested/pre-effect durable window.
  return stableFingerprintFixture(['session-terminal-cleanup-v1', sessionId]);
}

function sessionRow(database, sessionId) {
  const rows = sqliteRows(
    database,
    `SELECT aggregate_json FROM managed_session WHERE session_id=${sqlQuote(sessionId)}`,
  );
  assert.equal(rows.length, 1, `missing durable Session ${sessionId}`);
  assert.ok(rows[0].aggregate_json, `Session ${sessionId} has no canonical aggregate`);
  const envelope = parseDurableJson(rows[0].aggregate_json);
  // Fresh roots use the Session Store's current v2 writer. Historical v1
  // normalization has its own exact upgrade test and is not this fault seam.
  assert.equal(envelope.format, 'awaken.session.v2');
  const aggregate = envelope.aggregate;
  return {
    envelope,
    aggregate,
    status: aggregate.status,
    resources: aggregate.resources,
  };
}

function requiredRealizationLease(database, sessionId) {
  const lease = sessionRow(database, sessionId).aggregate.realization;
  assert.ok(lease, `Session ${sessionId} has no durable realization lease`);
  assert.ok(lease.runtime_incarnation, `Session ${sessionId} has no Runtime incarnation`);
  return lease;
}

function terminalCleanupReceiptRecorded(aggregate, sessionId) {
  const cleanup = aggregate.terminal_cleanup;
  return cleanup?.state === 'completed'
    || cleanup?.completions?.[sessionId]?.thread_id === sessionId;
}

function updateSessionRow(database, sessionId, status, resources) {
  const row = sessionRow(database, sessionId);
  // Fault-injection constraint: mutate only current aggregate-owned fields.
  // Archive visibility belongs to `disposition`; fabricating the retired
  // `archived_at` compatibility field would turn a recovery case into corrupt
  // storage before the behavior under test can run.
  const aggregate = {
    ...row.aggregate,
    status,
    resources,
  };
  sqliteExec(
    database,
    `UPDATE managed_session SET aggregate_json=${sqlQuote(stringifyDurableJson({
      ...row.envelope,
      aggregate,
    }))}
       WHERE session_id=${sqlQuote(sessionId)}`,
  );
}

function persistPreparedGeneration(database, sessionId) {
  const row = sessionRow(database, sessionId);
  assert.equal(row.status, 'idle');
  assert.equal(row.resources.pending, undefined);
  const revision = row.resources.revision + 1;
  const previous = row.resources.activations.map((activation) => ({
    ...activation,
    state: activation.state === 'active' ? 'releasing' : activation.state,
  }));
  const prepared = row.resources.activations
    .filter((activation) => activation.state === 'active')
    .map((activation) => ({
      ...activation,
      activation_id: `${sessionId}:${revision}:${activation.binding_id}`,
      revision,
      state: 'prepared',
      attempts: 0,
      last_error: 'process died before realization',
    }));
  assert.ok(prepared.length > 0);
  updateSessionRow(database, sessionId, 'idle', {
    revision,
    active: row.resources.active,
    pending: row.resources.active,
    activations: [...previous, ...prepared],
  });
}

function persistTerminalRelease(database, sessionId) {
  const row = sessionRow(database, sessionId);
  assert.equal(row.status, 'idle');
  const resources = {
    ...row.resources,
    activations: row.resources.activations.map((activation) => ({
      ...activation,
      state: activation.state === 'active' ? 'releasing' : activation.state,
    })),
  };
  assert.ok(resources.activations.some((activation) => activation.state === 'releasing'));
  const aggregate = {
    ...row.aggregate,
    status: 'terminated',
    resources,
    terminal_cleanup: {
      // The production quiescence owner must freeze its actual Runtime cursor
      // and thread prefix. A hand-authored Requested without that cursor is
      // historical unproven state, not today's post-quiescence crash boundary.
      state: 'fenced',
      effect_id: terminalCleanupEffectId(sessionId),
    },
  };
  sqliteExec(
    database,
    `UPDATE managed_session SET aggregate_json=${sqlQuote(stringifyDurableJson({
      ...row.envelope,
      aggregate,
    }))}
       WHERE session_id=${sqlQuote(sessionId)}`,
  );
}

async function main() {
  // Constraints/invariants for the decision tables below: the persisted Session
  // activation/cleanup state is the only recovery authority; listener readiness
  // and process liveness cannot themselves claim an effect complete or
  // authorize a second physical attempt.
  const directory = fs.mkdtempSync(path.join(os.tmpdir(), 'awaken-activation-recovery-'));
  const upstream = await startFakeAnthropic(FAKE_KEY, { models: [MODEL] });
  const sessionsDatabase = path.join(directory, 'sessions.db');
  const registryDatabase = path.join(directory, 'worker-registry.db');
  let server = start(directory);
  try {
    await ready();
    await authorModel(upstream);
    const client = managedWorkspaceClient(`http://127.0.0.1:${PORT}`, WORKSPACE);

    // SDK-admission decision rule: a typed Session call with an unsupported
    // Resource reaches the public Managed route and is rejected with 400 before
    // any aggregate or recovery work exists.
    await assert.rejects(
      () => client.beta.sessions.create({
        agent: AGENT,
        resources: [{ type: 'unsupported-resource' }],
        betas: [MANAGED_BETA],
      }),
      (error) => error?.status === 400,
    );

    // A Workdir-backed MemoryStore exercises the same activation state machine
    // without claiming the local sandbox can enforce a read-only File mount.
    // File isolation belongs to the namespace/container suites.
    const memoryStore = await client.beta.memoryStores.create({
      name: 'activation-recovery-memory',
      betas: [MEMORY_BETA],
    });
    const memoryStoreId = memoryStore.id;
    const recovering = await client.beta.sessions.create({
      agent: AGENT,
      environment_id: 'env_local',
      resources: [{
        type: 'memory_store', memory_store_id: memoryStoreId,
      }],
      betas: [MANAGED_BETA],
    });

    const terminating = await client.beta.sessions.create({
      agent: AGENT,
      environment_id: 'env_local',
      resources: [{
        type: 'memory_store', memory_store_id: memoryStoreId,
      }],
      betas: [MANAGED_BETA],
    });

    // Initial-realization cause/effect table: C1 listener ready; C2 a real Run
    // is claimed by the registered Worker; C3 every initial Resource generation
    // is Active. C1 alone legitimately leaves a demand-driven remote projection
    // `preparing`; only C1+C2+C3 is a valid baseline for injecting a later
    // replacement/release crash window.
    //
    // | Rule | C1 | C2 | C3 | Effect |
    // |---|---|---|---|---|
    // | B1 | yes | no | no | retain Prepared; no Coordinator-side effect |
    // | B2 | yes | yes | yes | stable idle baseline; crash injection is valid |
    const baselineSessionIds = [
      recovering.id,
      terminating.id,
    ];
    for (const sessionId of baselineSessionIds) {
      await driveSession(client, sessionId, `establish active baseline for ${sessionId}`);
    }
    await waitForValue(
      () => baselineSessionIds.map((sessionId) => sessionRow(sessionsDatabase, sessionId)),
      (rows) => rows.every((row) => row.status === 'idle'
        && row.resources.pending === undefined
        && row.resources.activations.every((activation) => activation.state === 'active')),
      'initial Resource generations did not converge before crash injection',
      { timeoutMs: 45_000 },
    );

    const terminalSessionIds = [terminating.id];
    const predecessorLeases = new Map(terminalSessionIds.map((sessionId) => [
      sessionId,
      requiredRealizationLease(sessionsDatabase, sessionId),
    ]));

    // Model process death after each first durable edge: Prepared for a live
    // replacement, and Fenced root cleanup + Releasing resources for a terminal
    // Session. After real quiescence below, a second crash retains the actual
    // Requested scope, including its authoritative Runtime commit cursor.
    const crashedProcess = server;
    assert.equal(crashedProcess.exitCode, null, 'original process is live before the crash');
    assert.equal(crashedProcess.signalCode, null, 'original process has no prior exit');
    await hardKillServer(crashedProcess);
    assert.equal(crashedProcess.signalCode, 'SIGKILL', 'observe the exact original child exit');
    persistPreparedGeneration(sessionsDatabase, recovering.id);
    persistTerminalRelease(sessionsDatabase, terminating.id);

    server = start(directory);
    await ready();

    // Recovery cause/effect graph: listener readiness starts the authoritative
    // Coordinator supervisor but cannot perform Worker-owned physical effects.
    // A Prepared remote generation (C1) therefore remains pending until a real
    // Run demand is claimed (C2), then the Worker realizes that exact generation
    // (E1). A terminal Releasing generation (C3) is Coordinator cleanup work and
    // converges in the background (E2).
    //
    // Decision table:
    // | Rule | durable state | HTTP ready | Run demand | required observation |
    // | R1   | Prepared      | yes        | no | remains Prepared, no attempt |
    // | R2   | Prepared      | yes        | yes | Active + no pending |
    // | R3   | Releasing     | yes        | n/a | Released in background |
    //
    // Replacement timing rule: SIGKILL leaves the previous Session Work lease
    // live for its canonical 60-second TTL. Before expiry the replacement must
    // not steal it and R2 remains Prepared with attempts=0; after expiry the
    // same queued demand authorizes takeover and R2 must settle. The 90-second
    // observation bound covers that lease plus registration/claim latency; it
    // does not add a second demand or relax the final state oracle.
    //
    await sleep(500);
    const demandPending = sessionRow(sessionsDatabase, recovering.id);
    assert.ok(demandPending.resources.pending, 'R1 retains the durable generation');
    assert.equal(demandPending.resources.activations.at(-1).attempts, 0, 'R1 has no hidden effect');
    await driveSession(
      client,
      recovering.id,
      'recover prepared Resource generation',
      { timeoutMs: 90_000 },
    );

    // The bounded durable-state waits cover R2/R3 without creating a second
    // readiness contract or racing the sole supervisor/Worker claim path.
    const recovered = await waitForValue(
      () => sessionRow(sessionsDatabase, recovering.id),
      (row) => row.status === 'idle'
        && row.resources.pending === undefined
        && row.resources.activations.map((activation) => activation.state).join(',')
          === 'released,active',
      'prepared resource generation did not recover',
      { timeoutMs: 90_000 },
    );
    assert.equal(recovered.status, 'idle');
    assert.equal(recovered.resources.pending, undefined);
    assert.deepEqual(
      recovered.resources.activations.map((activation) => activation.state),
      ['released', 'active'],
    );
    assert.equal(recovered.resources.activations[1].attempts, 1);
    assert.equal(recovered.resources.activations[1].last_error, undefined);

    // Q1 Fenced + committed ended Runs -> the existing Coordinator quiescence
    // freezes the exact targets/cursor and commits Requested. Q2 no independent
    // predecessor exit -> it cannot prepare/dispose resources. Crash again at
    // this real Requested boundary: restart must resume that identical scope,
    // rather than the fixture fabricating a cursor, completion or no-Create.
    const requested = await waitForValue(
      () => sessionRow(sessionsDatabase, terminating.id),
      (row) => row.aggregate.terminal_cleanup.state === 'requested'
        && row.aggregate.terminal_cleanup.runtime_commit_cursor !== undefined,
      'terminal quiescence did not freeze its actual Runtime scope',
    );

    // Exit-admission cause/effect table (the production affinity owner is
    // session_terminal_cleanup_affinity_for; Registry owns the retained fact):
    // X1 expired predecessor + Requested cleanup + no process-exit receipt ->
    // retain the original lease and Releasing generation, even after another
    // Session's successful replacement proves the new Worker is available.
    // X2 the same state + exact independently observed original process exit ->
    // claim a higher epoch, prepare/dispose, and record the original receipt.
    // Constraints: this is the existing SQLite crash-injection seam, not an
    // AllInOne production exit observer. The fixture parent actually joined
    // SIGKILL of the process containing the embedded Worker. It writes only
    // that incarnation's closure/exit fields; expiry, resource/source identity,
    // cleanup targets and every other Registry field remain untouched. A new
    // boot, lease timeout or successful unrelated Run supplies no exit proof.
    // Exact/foreign/missing/live/malformed receipt combinations are covered by
    // independently_exited_predecessor_allows_terminal_assignment_without_cleanup_ack.
    const predecessor = predecessorLeases.get(terminating.id);
    const unclaimed = sessionRow(sessionsDatabase, terminating.id);
    assert.deepEqual(unclaimed.aggregate.realization, predecessor, 'X1 cannot steal the predecessor');
    assert.equal(unclaimed.aggregate.terminal_cleanup.state, 'requested', 'X1 retains intent');
    assert.ok(unclaimed.resources.activations.every((activation) => activation.state === 'releasing'));
    assert.ok(predecessor.expires_at_unix_ms <= Date.now(), 'X1 observes actual lease expiry');
    const requestedProcess = server;
    assert.equal(requestedProcess.exitCode, null, 'Requested crash process is live');
    assert.equal(requestedProcess.signalCode, null, 'Requested crash has no prior exit');
    await hardKillServer(requestedProcess);
    assert.equal(requestedProcess.signalCode, 'SIGKILL', 'join the real Requested crash');
    server = start(directory);
    await ready();
    const retained = sessionRow(sessionsDatabase, terminating.id);
    assert.deepEqual(retained.aggregate.terminal_cleanup, requested.aggregate.terminal_cleanup,
      'Requested restart preserves the production-frozen scope and cursor');
    assert.deepEqual(retained.aggregate.realization, predecessor,
      'Requested restart cannot borrow a new incarnation as exit evidence');
    const oldWorkers = sqliteRows(registryDatabase,
      'SELECT worker_id, incarnation_id, generation, state, process_exit_json '
        + 'FROM worker_registry_worker WHERE worker_id = ?', predecessor.owner)
      .filter((row) => `${row.worker_id}:${row.generation}:${row.incarnation_id}`
        === predecessor.runtime_incarnation);
    assert.equal(oldWorkers.length, 1, 'one exact historical Worker incarnation');
    const oldWorker = oldWorkers[0];
    assert.equal(oldWorker.process_exit_json, null, 'X1 has no independent exit receipt');
    const exitReceipt = {
      provider_id: 'node-child-process-fixture',
      execution_ref: predecessor.runtime_incarnation,
      evidence_ref: `child:${crashedProcess.pid}:signal:${crashedProcess.signalCode}`,
    };
    const closed = sqliteRun(registryDatabase,
      "UPDATE worker_registry_worker SET state = 'dead', process_exit_json = ? "
        + 'WHERE worker_id = ? AND incarnation_id = ? AND generation = ? '
        + 'AND state = ? AND process_exit_json IS NULL',
      JSON.stringify(exitReceipt), oldWorker.worker_id, oldWorker.incarnation_id,
      oldWorker.generation, oldWorker.state);
    assert.equal(closed.changes, 1, 'X2 closes only the exact observed predecessor');

    // Cold-replacement decision table: C1 terminal cleanup is Requested with
    // no resident Host slot after SIGKILL; C2 the registry has a fresh process
    // incarnation for the same logical Worker; C3 no Run claim or second API
    // command exists for a terminal Session. Effects: E1 claim-next fences epoch
    // N+1 on the Session root; E2 the Worker installs the frozen projection,
    // polls the aggregate command, and records the root receipt before the
    // Resource generation becomes Released.
    // The observation follows the scenario's unrelated prepared-generation
    // demand so it does not block the sole lifecycle supervisor from starting.
    //
    // | Rule | terminal | predecessor | replacement | Effect |
    // |---|---|---|---|---|
    // | G48-1 | yes | process dead | same owner/new incarnation | E1 + E2 |
    // | G48-2 | yes | process dead | no terminal Run/API demand | E1 + E2 |
    await waitForValue(
      () => terminalSessionIds.map((sessionId) => ({
        sessionId,
        row: sessionRow(sessionsDatabase, sessionId),
      })),
      (rows) => rows.every(({ sessionId, row }) => {
        const predecessor = predecessorLeases.get(sessionId);
        const replacement = row.aggregate.realization;
        return replacement?.owner === predecessor.owner
          && replacement.runtime_incarnation !== predecessor.runtime_incarnation
          && replacement.epoch > predecessor.epoch
          && terminalCleanupReceiptRecorded(row.aggregate, sessionId);
      }),
      'cold replacement did not claim and receipt every terminal root cleanup',
      { timeoutMs: 90_000 },
    ).catch((error) => {
      // D1 a bounded release failure -> retain only the existing Dispatch and
      // Registry admission coordinates before fixture cleanup. These reads do
      // not close a physical attempt or replace the original failure. Omit
      // request bodies, credentials and source contents from diagnostics.
      try {
        console.error('E2E cleanup admission observations:', JSON.stringify({
          dispatch: sqliteRows(path.join(directory, 'dispatch.db'),
            'SELECT run_id, thread_id, status, active_attempt_owner, active_attempt_epoch, '
              + 'active_attempt_closing FROM runtime_dispatch WHERE thread_id = ?', terminating.id),
          workers: sqliteRows(registryDatabase,
            'SELECT worker_id, incarnation_id, generation, state, expires_at_ms, process_exit_json, '
              + "json_extract(manifest_json, '$.capabilities') AS capabilities "
              + 'FROM worker_registry_worker WHERE worker_id = ? ORDER BY generation', predecessor.owner),
        }));
      } catch (diagnosticError) {
        console.error('E2E cleanup admission observation unavailable:', diagnosticError.name);
      }
      throw error;
    });

    // The original cold-replacement budget includes Registry lease expiry and
    // registration. HTTP readiness cannot satisfy that barrier. Only after
    // its exact cleanup receipt may the shorter Resource consistency wait run;
    // no timeout or final state requirement is expanded.
    const released = await waitForValue(
      () => sessionRow(sessionsDatabase, terminating.id),
      (row) => row.resources.pending === undefined
        && row.resources.activations.every((activation) => activation.state === 'released'),
      'terminal resource generation did not release',
    );
    assert.equal(released.status, 'terminated');
    assert.equal(released.resources.pending, undefined);
    assert.ok(released.resources.activations.every((activation) => activation.state === 'released'));

    console.log('E2E PASS: Prepared and Releasing resource generations recover after process death.');
  } finally {
    await stop(server).catch(() => {});
    await upstream.close();
    cleanupFixtureTree(directory);
  }
}

main().catch((error) => {
  console.error('E2E FAIL:', error);
  process.exitCode = 1;
});
