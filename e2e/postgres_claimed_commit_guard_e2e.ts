// Postgres claim/commit fencing over the real HTTP server and a real database.
//
// A database trigger delays ThreadCommit while /commit-claimed holds its opaque
// dispatch guard. A concurrent reclaimer must SKIP the locked row; after the
// commit returns it may recover the expired lease under a higher epoch, but it
// cannot duplicate committed history and the old epoch cannot settle.

import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import fs, { mkdtempSync } from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { spawnServer, stopServer, waitForPort } from './harness.mjs';

const DATABASE_URL = process.env.AWAKEN_DATABASE_URL;
const POSTGRES_CONTAINER = process.env.AWAKEN_E2E_POSTGRES_CONTAINER;
if (!DATABASE_URL) {
  console.error('SKIP: postgres_claimed_commit_guard_e2e requires AWAKEN_DATABASE_URL');
  process.exit(0);
}

const PORT = Number(process.env.E2E_PORT ?? 38814);
const BASE = `http://127.0.0.1:${PORT}`;
const THREAD = 'pg-claimed-guard-ts';
const OWNER_A = 'pg-worker-a';
const OWNER_B = 'pg-worker-b';
const identities = new Map<string, unknown>();

function sql(statement: string): string {
  const command = POSTGRES_CONTAINER ? 'docker' : 'psql';
  const args = POSTGRES_CONTAINER
    ? [
        'exec',
        POSTGRES_CONTAINER,
        'psql',
        '-U',
        'postgres',
        '-d',
        'awaken',
        '-X',
        '-v',
        'ON_ERROR_STOP=1',
        '-At',
        '-c',
        statement,
      ]
    : [DATABASE_URL!, '-X', '-v', 'ON_ERROR_STOP=1', '-At', '-c', statement];
  return execFileSync(command, args, { encoding: 'utf8' }).trim();
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
      manifest: {
        manifest_version: 1,
        build_digest: 'postgres-claimed-commit-guard-e2e',
        capabilities: ['host-executor/v1', 'native-runtime'],
        zone: null,
        architecture: process.arch,
        sandbox: {
          isolation: 'workdir', tool_transparent: false, path_fidelity: false,
          enforced_readonly: false, network_isolation: false,
          secret_egress_substitution: false, resource_limits: false, custom_rootfs: false,
        },
        sandbox_backends: [],
        dispatch_contract: { min: 1, max: 1 },
        runtime_protocol: { min: 1, max: 1 },
        checkpoint_formats: ['stream-v1'],
        capacity: { max_concurrent: 1, resources: {} },
      },
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
}

function terminalCommit(runId: string) {
  return {
    thread_id: THREAD,
    run_fact: { run_id: runId, phase: { Ended: 'NaturalEnd' } },
    messages: [
      {
        id: `pg-guard-${runId}`,
        role: 'Assistant',
        content: [{ type: 'text', text: 'atomic claimed commit' }],
      },
    ],
    state: [],
    events: [],
    waiting: null,
  };
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

async function claimEventually(owner: string, timeoutMs = 5_000): Promise<any> {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() <= deadline) {
    const response = await post('/v1/worker/dispatch/claim', {}, owner);
    assert.equal(response.status, 200, response.text);
    if (response.json?.claimed) return response.json.claimed;
    await new Promise((resolve) => setTimeout(resolve, 25));
  }
  throw new Error('the expired run did not become claimable after the epoch guard was released');
}

async function main(): Promise<void> {
  const storage = mkdtempSync(path.join(tmpdir(), 'awaken-pg-claimed-guard-'));
  const server = spawnServer('echo', PORT, {
    AWAKEN_INGRESS: 'durable',
    AWAKEN_DISPATCH_BACKEND: 'postgres',
    AWAKEN_STORE: 'postgres',
    AWAKEN_DATABASE_URL: DATABASE_URL!,
    AWAKEN_STORAGE_DIR: storage,
    AWAKEN_DISABLE_LOCAL_POOL: '1',
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

    const committing = post(
      '/v1/worker/commit-claimed',
      {
        claim: { run_id: runId, owner: first.lease.owner, epoch: first.lease.epoch },
        commit: terminalCommit(runId),
      },
      OWNER_A,
    );
    await waitUntilCommitIsBlocked();

    // Remove A from placement while its already-authorized commit is in flight.
    // B is now the only eligible worker, so the null claim below is evidence of
    // the database epoch guard (not the placement policy preferring A).
    const draining = await post('/v1/worker/drain', {}, OWNER_A);
    assert.equal(draining.status, 200, draining.text);
    assert.equal(draining.json?.mutation, 'applied');

    const blockedReclaim = await post('/v1/worker/dispatch/claim', {}, OWNER_B);
    assert.equal(blockedReclaim.status, 200, blockedReclaim.text);
    assert.equal(
      blockedReclaim.json?.claimed,
      null,
      'FOR UPDATE guard prevents re-ownership while ThreadCommit is in flight',
    );

    const committed = await committing;
    assert.equal(committed.status, 200, `current claimed commit succeeds: ${committed.text}`);
    sql('DROP TRIGGER IF EXISTS e2e_delay_claimed_commit_trigger ON runtime_commit');
    sql('DROP FUNCTION IF EXISTS e2e_delay_claimed_commit()');

    // sqlx queues rollback when the opaque transaction guard is dropped, so the
    // HTTP response may arrive just before PostgreSQL releases the row lock. A
    // real worker polls the queue; mirror that behavior with a bounded deadline.
    const recovered = await claimEventually(OWNER_B);
    assert.equal(recovered?.lease?.run_id, runId, 'worker B recovers the same run after guard release');
    assert.ok(recovered.lease.epoch > first.lease.epoch, 'recovery increments the monotone fencing epoch');

    const settled = await post(
      '/v1/worker/dispatch/settle',
      { run_id: runId, epoch: recovered.lease.epoch, outcome: 'Done', consumed: [] },
      OWNER_B,
    );
    assert.equal(settled.status, 200, settled.text);
    assert.equal(settled.json?.settled, true, 'the recovered owner settles the terminal dispatch');
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
      'POSTGRES CLAIMED-COMMIT GUARD TS E2E PASS: in-flight commit excluded reclaim, recovery advanced epoch, stale settle was fenced and history committed once.',
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
