// Postgres claim/commit fencing over the real HTTP server and a real database.
//
// A database trigger delays ThreadCommit while /commit-claimed holds its opaque
// dispatch guard. A concurrent reclaimer must SKIP the locked row; after the
// commit returns it may recover the expired lease under a higher epoch, but it
// cannot duplicate committed history and the old epoch cannot settle.

import assert from 'node:assert/strict';
import fs, { mkdtempSync } from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { postgresQuery } from './postgres.mjs';
import { postgresDatabaseName, spawnServer, stopServer, waitForPort } from './harness.mjs';
import {
  claimedCommitRequestFixture,
  terminalThreadCommitFixture,
} from './fixtures/thread_commit_fixture.mjs';
import {
  workdirWorkerManifestFixture,
  WORKER_INHERITED_TOOLSETS_CAPABILITY,
} from './fixtures/worker_manifest_fixture.mjs';

const DATABASE_URL = process.env.SESSION_DEPLOYMENT_DATABASE_URL;
const POSTGRES_CONTAINER = process.env.AWAKEN_E2E_POSTGRES_CONTAINER;
if (!DATABASE_URL) {
  console.error('SKIP: postgres_claimed_commit_guard_e2e requires SESSION_DEPLOYMENT_DATABASE_URL');
  process.exit(0);
}
const PROBE_DATABASE = postgresDatabaseName(DATABASE_URL);

const PORT = Number(process.env.E2E_PORT ?? 38814);
const BASE = `http://127.0.0.1:${PORT}`;
const THREAD = 'pg-claimed-guard-ts';
const OWNER_A = 'pg-worker-a';
const OWNER_B = 'pg-worker-b';
const identities = new Map<string, unknown>();
const heartbeatSequences = new Map<string, number>();

function sql(statement: string): string {
  return postgresQuery({container:POSTGRES_CONTAINER,database:PROBE_DATABASE,url:DATABASE_URL},statement);
}

function quoted(value: string): string {
  return value.replaceAll("'", "''");
}

async function post(pathname: string, body: unknown, worker?: string) {
  const headers: Record<string, string> = { 'content-type': 'application/json' };
  if (worker) headers['x-awaken-worker-id'] = worker;
  const payload = worker && pathname !== '/v1/worker/register'
    ? { ...(body as Record<string, unknown>), identity: identities.get(worker) }
    : body;
  const response = await fetch(`${BASE}${pathname}`, {
    method: 'POST',
    headers,
    body: JSON.stringify(payload),
  });
  const text = await response.text();
  let json: any = null;
  try {
    json = JSON.parse(text);
  } catch {
    // Preserve the raw response for assertions below.
  }
  return { status: response.status, json, text };
}

async function registerReadyWorker(worker: string): Promise<void> {
  const registration = await post('/v1/worker/register', {
    registration: {
      worker_id: worker,
      incarnation_id: `${worker}-${process.pid}`,
      manifest: workdirWorkerManifestFixture({
        buildDigest: 'postgres-claimed-commit-guard-e2e',
        // The public background route now admits one Session-owned root. Both
        // contenders must be able to realize its frozen Resource generation;
        // otherwise placement correctly yields no claim before this test ever
        // reaches the Postgres commit-fence decision table. The fresh root also
        // freezes the child-toolset format, which the shipped Worker advertises.
        capabilities: [
          'host-executor/v1', 'native-runtime', 'session-resources/v1',
          WORKER_INHERITED_TOOLSETS_CAPABILITY,
        ],
      }),
    },
  }, worker);
  assert.equal(registration.status, 200, registration.text);
  const identity = registration.json?.worker?.snapshot?.identity;
  assert.ok(identity, `registration returns an identity for ${worker}`);
  identities.set(worker, identity);
  const heartbeat = await post('/v1/worker/heartbeat', {
    heartbeat: { sequence: 1, ready: true, in_flight: 0 },
  }, worker);
  assert.equal(heartbeat.status, 200, heartbeat.text);
  assert.equal(heartbeat.json?.mutation, 'applied');
  heartbeatSequences.set(worker, 1);
}

async function heartbeatWorker(worker: string, ready: boolean): Promise<void> {
  const sequence = (heartbeatSequences.get(worker) ?? 0) + 1;
  const response = await post('/v1/worker/heartbeat', {
    heartbeat: { sequence, ready, in_flight: worker === OWNER_A ? 1 : 0 },
  }, worker);
  assert.equal(response.status, 200, `${worker} heartbeat: ${response.text}`);
  assert.equal(response.json?.mutation, 'applied', `${worker} stays registered`);
  heartbeatSequences.set(worker, sequence);
}

async function waitUntilCommitIsBlocked(timeoutMs = 5_000): Promise<void> {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() <= deadline) {
    const active = Number(
      sql(
        "SELECT count(*) FROM pg_stat_activity WHERE datname=current_database() " +
          "AND state='active' AND query LIKE 'INSERT INTO runtime_commit%'",
      ),
    );
    if (active >= 1) return;
    await new Promise((resolve) => setTimeout(resolve, 25));
  }
  throw new Error('the delayed Postgres ThreadCommit never entered its transaction');
}

async function claimOrCompleteEventually(
  owner: string,
  runId: string,
  timeoutMs = 120_000,
): Promise<{ claimed: any | null; completed: boolean }> {
  // Causes: C1 the Postgres row guard releases; C2 the Session Work lease is
  // still held by drained A; C3 B or the terminal scanner next acquires that
  // independent authority. Effects: E1 no premature B claim; E2 B's higher
  // epoch claim OR the exact completion tombstone closes the wait. Rules
  // C1+C2=>E1, C1+C3=>E2. Five seconds covered only the row lock, not Work.
  const deadline = Date.now() + timeoutMs;
  let nextHeartbeat = Date.now() + 10_000;
  while (Date.now() <= deadline) {
    if (Date.now() >= nextHeartbeat) {
      await heartbeatWorker(OWNER_A, false);
      await heartbeatWorker(OWNER_B, true);
      nextHeartbeat = Date.now() + 10_000;
    }
    const response = await post('/v1/worker/dispatch/claim', {}, owner);
    assert.equal(response.status, 200, response.text);
    if (response.json?.claimed) return { claimed: response.json.claimed, completed: false };
    if (Number(sql(`SELECT count(*) FROM runtime_dispatch_completion WHERE run_id='${quoted(runId)}'`)) === 1) {
      return { claimed: null, completed: true };
    }
    await new Promise((resolve) => setTimeout(resolve, 500));
  }
  throw new Error(
    'the expired run neither became claimable nor completed after both fences: ' +
      `dispatch=${sql(`SELECT status, lease_owner, lease_epoch, active_attempt_owner, active_attempt_epoch, active_attempt_closing, worker_assignment FROM runtime_dispatch WHERE run_id='${quoted(runId)}'`)} ` +
      `completion=${sql(`SELECT count(*) FROM runtime_dispatch_completion WHERE run_id='${quoted(runId)}'`)}`,
  );
}

async function main(): Promise<void> {
  const storage = mkdtempSync(path.join(tmpdir(), 'awaken-pg-claimed-guard-'));
  const server = spawnServer('echo', PORT, {
    SESSION_DEPLOYMENT_INGRESS: 'durable',
    SESSION_DEPLOYMENT_DISPATCH_BACKEND: 'postgres',
    SESSION_DEPLOYMENT_STORE: 'postgres',
    SESSION_DEPLOYMENT_DATABASE_URL: DATABASE_URL!,
    SESSION_DEPLOYMENT_STORAGE_DIR: storage,
    SESSION_DEPLOYMENT_DISABLE_LOCAL_POOL: '1',
  }).server;
  try {
    await waitForPort(PORT);
    await registerReadyWorker(OWNER_A);
    await registerReadyWorker(OWNER_B);
    const submitted = await post(`/v1/durable/threads/${THREAD}/submit_background`, {
      text: 'prove the commit epoch guard',
    });
    assert.equal(submitted.status, 200, submitted.text);

    const firstClaim = await post('/v1/worker/dispatch/claim', {}, OWNER_A);
    assert.equal(firstClaim.status, 200, firstClaim.text);
    const first = firstClaim.json?.claimed;
    assert.ok(first, 'worker A claimed the Postgres dispatch');
    const runId = first.lease.run_id as string;

    // A keeps authority for its issued claim but is not eligible for a new
    // placement. A live not-ready heartbeat lets the later stale-epoch request
    // reach the dispatch fence instead of failing merely on registry expiry.
    // Set it before the delayed transaction so this control request cannot
    // consume the trigger's bounded delay.
    await heartbeatWorker(OWNER_A, false);

    // Make the lease recoverable, then delay only this run's commit. The guard is
    // allowed because A still owns the current epoch; reclaim cannot interleave
    // after that validation because the guard retains FOR UPDATE until commit ends.
    sql(`UPDATE runtime_dispatch SET lease_until=0 WHERE run_id='${quoted(runId)}'`);
    sql(`
      CREATE OR REPLACE FUNCTION e2e_delay_claimed_commit() RETURNS trigger AS $$
      BEGIN
        IF NEW.run_id = '${quoted(runId)}' THEN PERFORM pg_sleep(2); END IF;
        RETURN NEW;
      END;
      $$ LANGUAGE plpgsql;
      DROP TRIGGER IF EXISTS e2e_delay_claimed_commit_trigger ON runtime_commit;
      CREATE TRIGGER e2e_delay_claimed_commit_trigger BEFORE INSERT ON runtime_commit
      FOR EACH ROW EXECUTE FUNCTION e2e_delay_claimed_commit();
    `);

    // Cause-effect graph / decision table:
    // C1 current epoch + C2 valid canonical operation -> commit holds row lock;
    // while held, C3 reclaim -> no claim; after release -> higher epoch claim;
    // the old epoch can neither settle nor duplicate the transcript.
    //
    // | Rule | Commit guard | Reclaim time | Epoch used | Result              |
    // | P1   | held         | during       | new        | null or wait; never re-own |
    // | P2   | released     | after Work expiry | new | B claim or scanner completion |
    // | P3   | released     | after        | old        | stale settle fenced |
    const committing = post(
      '/v1/worker/commit-claimed',
      claimedCommitRequestFixture({
        claimed: first,
        commit: terminalThreadCommitFixture({
          runId,
          threadId: THREAD,
          messageId: `pg-guard-${runId}`,
          text: 'atomic claimed commit',
        }),
        ordinal: 0,
        expectedThreadVersion: 0,
      }),
      OWNER_A,
    );
    await waitUntilCommitIsBlocked();

    // B is the only eligible worker. A store may use SKIP LOCKED and immediately
    // return null, or wait on the row lock; neither may re-own the row before the
    // guarded commit releases it.
    const reclaiming = post('/v1/worker/dispatch/claim', {}, OWNER_B);
    const earlyReclaim = await Promise.race([
      reclaiming.then((response) => ({ type: 'response' as const, response })),
      new Promise<{ type: 'pending' }>((resolve) =>
        setTimeout(() => resolve({ type: 'pending' }), 250)),
    ]);
    if (earlyReclaim.type === 'response') {
      assert.equal(earlyReclaim.response.status, 200, earlyReclaim.response.text);
      assert.equal(
        earlyReclaim.response.json?.claimed,
        null,
        'the commit guard forbids re-ownership while the transaction is in flight',
      );
    }

    const committed = await committing;
    assert.equal(committed.status, 200, `current claimed commit succeeds: ${committed.text}`);
    sql('DROP TRIGGER IF EXISTS e2e_delay_claimed_commit_trigger ON runtime_commit');
    sql('DROP FUNCTION IF EXISTS e2e_delay_claimed_commit()');

    // sqlx queues rollback when the opaque transaction guard is dropped, so the
    // HTTP response may arrive just before PostgreSQL releases the row lock.
    // Session Work can outlive that lock; B and the terminal scanner race for
    // the same committed Ended truth only after its independent fence clears.
    const reclaimResponse = await reclaiming;
    assert.equal(reclaimResponse.status, 200, reclaimResponse.text);
    const recovery = reclaimResponse.json?.claimed
      ? { claimed: reclaimResponse.json.claimed, completed: false }
      : await claimOrCompleteEventually(OWNER_B, runId);
    if (recovery.claimed) {
      const recovered = recovery.claimed;
      assert.equal(recovered.lease.run_id, runId, 'worker B recovers the same run after guard release');
      assert.ok(recovered.lease.epoch > first.lease.epoch, 'recovery increments the fencing epoch');
      const settled = await post(
        '/v1/worker/dispatch/settle',
        { run_id: runId, epoch: recovered.lease.epoch, outcome: 'Done', consumed: [] },
        OWNER_B,
      );
      assert.equal(settled.status, 200, settled.text);
      assert.equal(settled.json?.settled, true, 'B settles the terminal dispatch');
    } else {
      assert.equal(recovery.completed, true, 'the terminal scanner completed the exact Run');
      assert.equal(
        Number(sql(`SELECT count(*) FROM runtime_dispatch_completion WHERE run_id='${quoted(runId)}'`)),
        1,
        'the scanner wrote one exact completion tombstone',
      );
    }
    const stale = await post(
      '/v1/worker/dispatch/settle',
      { run_id: runId, epoch: first.lease.epoch, outcome: 'Done', consumed: [] },
      OWNER_A,
    );
    assert.equal(stale.status, 200, stale.text);
    assert.equal(stale.json?.settled, false, 'the superseded epoch cannot settle');

    const messages = await fetch(`${BASE}/v1/durable/threads/${THREAD}/messages`);
    assert.equal(messages.status, 200);
    const committedMessages = ((await messages.json()) as any).messages ?? [];
    assert.equal(
      committedMessages.filter((message: any) => String(message.text ?? '').includes('atomic claimed commit'))
        .length,
      1,
      'the guarded overlap commits one transcript effect',
    );

    console.log(
      'POSTGRES CLAIMED-COMMIT GUARD TS E2E PASS: in-flight commit excluded reclaim, terminal recovery respected Work, stale settle was fenced and history committed once.',
    );
  } finally {
    try {
      sql('DROP TRIGGER IF EXISTS e2e_delay_claimed_commit_trigger ON runtime_commit');
      sql('DROP FUNCTION IF EXISTS e2e_delay_claimed_commit()');
    } catch {
      // The disposable E2E database may not have completed migration on an early failure.
    }
    await stopServer(server).catch(() => {});
    fs.rmSync(storage, { recursive: true, force: true });
  }
}

main().catch((error) => {
  console.error('POSTGRES CLAIMED-COMMIT GUARD TS E2E FAIL:', error);
  process.exitCode = 1;
});
