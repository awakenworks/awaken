// The cross-node worker HTTP surface, end to end over the real server binary.
//
// A database-less worker drives runs and commits facts over HTTP — never opening
// the store. This exercises the two worker-facing seams the server now mounts:
//   - commit ingest  (POST /v1/worker/commit): the worker pushes a ThreadCommit,
//     the server (single writer) applies it; the fact reads back from the store,
//     and a redelivery is idempotent (at-least-once -> exactly-once effect).
//   - dispatch transport (POST /v1/worker/dispatch/claim): a worker claims runs
//     from the shared durable queue over HTTP; the endpoint is live and wired to
//     the real store (full enqueue->claim->settle semantics are proven in Rust).
//
// Run: node e2e/worker_transport_e2e.mjs

import assert from 'node:assert/strict';
import { mkdtempSync } from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { spawnServer, stopServer, waitForPort, pass } from './harness.mjs';

const PORT = Number(process.env.E2E_PORT ?? 38812);
const BASE = `http://127.0.0.1:${PORT}`;
const THREAD = 'worker-transport-1';
const ENV = {
  AWAKEN_INGRESS: 'durable',
  AWAKEN_STORAGE_DIR: mkdtempSync(path.join(tmpdir(), 'awaken-worker-transport-')),
  // Keep queued work available for this external worker instead of racing the
  // scenario host's in-process drain pool.
  AWAKEN_DISABLE_LOCAL_POOL: '1',
};
const WORKER = 'ts-worker-1';
let workerIdentity;

// The exact ThreadCommit wire shape (dumped from the neutral Rust types).
function threadCommit(runId = 'run-A', threadId = THREAD, text = 'hi from a db-less worker') {
  return {
    thread_id: threadId,
    run_fact: { run_id: runId, phase: { Ended: 'NaturalEnd' } },
    messages: [
      { id: `a-${runId}`, role: 'Assistant', content: [{ type: 'text', text }] },
    ],
    state: [],
    events: [],
    waiting: null,
  };
}

async function postJson(pathname, body, worker = WORKER) {
  const headers = { 'content-type': 'application/json' };
  if (worker !== null) headers['x-awaken-worker-id'] = worker;
  const payload = worker === WORKER && workerIdentity && pathname !== '/v1/worker/register'
    ? { ...body, identity: body.identity ?? workerIdentity }
    : body;
  const res = await fetch(`${BASE}${pathname}`, {
    method: 'POST',
    headers,
    body: JSON.stringify(payload),
  });
  const text = await res.text();
  let json;
  try {
    json = JSON.parse(text);
  } catch {
    json = null;
  }
  return { status: res.status, json, text };
}

async function registerReadyWorker() {
  const registration = await postJson('/v1/worker/register', {
    registration: {
      worker_id: WORKER,
      incarnation_id: `${WORKER}-${process.pid}`,
      manifest: {
        manifest_version: 1,
        build_digest: 'worker-transport-e2e',
        capabilities: ['credential-reference/v1', 'host-executor/v1', 'native-runtime'],
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
  });
  assert.equal(registration.status, 200, `worker registered: ${registration.text}`);
  workerIdentity = registration.json?.worker?.snapshot?.identity;
  assert.ok(workerIdentity, 'registration returns an incarnation-bound identity');
  const heartbeat = await postJson('/v1/worker/heartbeat', {
    heartbeat: { sequence: 1, ready: true, in_flight: 0 },
  });
  assert.equal(heartbeat.status, 200, `worker heartbeat accepted: ${heartbeat.text}`);
  assert.equal(heartbeat.json?.mutation, 'applied');
}

async function threadMessages(threadId = THREAD) {
  const res = await fetch(`${BASE}/v1/durable/threads/${threadId}/messages`);
  assert.equal(res.status, 200, 'durable thread messages readable');
  return (await res.json()).messages ?? [];
}

async function main() {
  const { server } = spawnServer('echo', PORT, ENV);
  try {
    await waitForPort(PORT);

    // Every worker route is authenticated. The local composition uses the
    // compatibility identity header; cloud replaces the authenticator with a
    // WorkerLease/mTLS implementation behind the same port.
    const anonymous = await postJson('/v1/worker/commit-claimed', {}, null);
    assert.equal(anonymous.status, 401, `anonymous worker is rejected: ${anonymous.text}`);
    pass('worker transport rejects a request with no authenticated identity');
    await registerReadyWorker();

    // --- dispatch transport: the claim endpoint is live and wired to the store ---
    // Queue a real run, then claim it over the authenticated transport. Legacy
    // owner/time/lease fields in the body are deliberately malicious: the server
    // must ignore them and derive authority from the verified identity + clock.
    const dispatchThread = 'worker-dispatch-auth-1';
    const queued = await postJson(`/v1/durable/threads/${dispatchThread}/submit_background`, {
      text: 'carry model grant to worker',
    }, null);
    assert.equal(queued.status, 200, `background run queued: ${queued.text}`);
    const claim = await postJson('/v1/worker/dispatch/claim', {
      owner: 'forged-owner',
      lease_ms: 9_999_999,
      now_ms: 1,
    });
    assert.equal(claim.status, 200, `dispatch claim endpoint live: ${claim.text}`);
    const claimed = claim.json?.claimed;
    assert.ok(claimed, `claim returns the queued run: ${claim.text}`);
    const leaseOwner = `${workerIdentity.worker_id}:${workerIdentity.generation}:${workerIdentity.incarnation_id}`;
    assert.equal(claimed.lease.owner, leaseOwner, 'claim owner comes from authenticated incarnation');
    assert.ok(claimed.lease.epoch >= 1, `claim carries a fencing epoch: ${claim.text}`);
    pass('dispatch claim binds owner to authenticated worker and returns a fencing epoch');

    // Re-enqueue the exact durable wire record with an opaque gateway grant. This
    // is the open-runtime side of secretless execution: persist/transport the
    // capability reference without interpreting it or carrying a provider key.
    const granted = structuredClone(claimed.request);
    granted.activation.run_id = `${claimed.request.activation.run_id}-grant`;
    granted.activation.thread_id = `${claimed.request.activation.thread_id}-grant`;
    granted.session_thread_id = granted.activation.thread_id;
    granted.execution_scope = 'scope-ts-17';
    granted.activation.snapshot.metadata = {
      source: { agent_id: '', revision: 0 },
      publication_version: '',
      resolution: { inputs: [] },
      fingerprint: '',
      inference_access: { scheme: 'credential-reference/v1', reference: 'grant-ts-17' },
    };
    granted.placement.required_capabilities = ['credential-reference/v1', 'native-runtime'];
    const enqueuedGrant = await postJson('/v1/worker/dispatch/enqueue', { request: granted });
    assert.equal(enqueuedGrant.status, 200, `grant-bearing dispatch enqueued: ${enqueuedGrant.text}`);

    // Complete the first ownership before claiming the next run.
    const settle = await postJson('/v1/worker/dispatch/settle', {
      run_id: claimed.lease.run_id,
      epoch: claimed.lease.epoch,
      outcome: 'Done',
      consumed: [],
    });
    assert.equal(settle.status, 200, `dispatch settle endpoint live: ${settle.text}`);
    assert.equal(settle.json?.settled, true, `current epoch settles: ${settle.text}`);

    const grantClaim = await postJson('/v1/worker/dispatch/claim', {});
    assert.equal(grantClaim.status, 200, `grant dispatch claimed: ${grantClaim.text}`);
    const grant = grantClaim.json?.claimed;
    assert.deepEqual(
      grant?.request?.activation?.snapshot?.metadata?.inference_access,
      { scheme: 'credential-reference/v1', reference: 'grant-ts-17' },
      'snapshot inference_access survives enqueue → durable store → authenticated claim unchanged',
    );
    assert.equal(
      grant?.request?.execution_scope,
      'scope-ts-17',
      'verified execution scope survives the durable worker boundary as an opaque coordinate',
    );
    assert.ok(!JSON.stringify(grant).includes('provider-key'), 'claim contains no provider credential');
    pass('secretless snapshot access survives durable dispatch without a provider key');

    // A different authenticated worker cannot commit the claim. The owner-bound
    // request is rejected before thread facts are applied.
    const claimedCommit = {
      claim: {
        run_id: grant.lease.run_id,
        owner: grant.lease.owner,
        epoch: grant.lease.epoch,
      },
      commit: threadCommit(
        grant.lease.run_id,
        grant.request.activation.thread_id,
        'claimed commit from authenticated worker',
      ),
    };
    const wrongOwner = await postJson('/v1/worker/commit-claimed', claimedCommit, 'worker-thief');
    assert.equal(wrongOwner.status, 401, `wrong claim owner rejected: ${wrongOwner.text}`);
    const committedClaim = await postJson('/v1/worker/commit-claimed', claimedCommit);
    assert.equal(committedClaim.status, 200, `current owner/epoch commits: ${committedClaim.text}`);
    assert.ok(typeof committedClaim.json?.sequence === 'number');
    const replay = await postJson('/v1/worker/commit-claimed', claimedCommit);
    assert.equal(replay.status, 200, `claimed commit redelivery accepted: ${replay.text}`);
    const committed = await threadMessages(grant.request.activation.thread_id);
    const mine = committed.filter((m) => (m.text ?? '').includes('claimed commit'));
    assert.equal(mine.length, 1, `claimed commit is idempotent: ${JSON.stringify(committed)}`);
    pass('commit-claimed enforces authenticated owner + epoch before applying facts');

    const grantSettle = await postJson('/v1/worker/dispatch/settle', {
      run_id: grant.lease.run_id,
      epoch: grant.lease.epoch,
      outcome: 'Done',
      consumed: [],
    });
    assert.equal(grantSettle.json?.settled, true, `grant dispatch settled: ${grantSettle.text}`);
    const stale = await postJson('/v1/worker/dispatch/settle', {
      run_id: grant.lease.run_id,
      epoch: grant.lease.epoch,
      outcome: 'Done',
      consumed: [],
    });
    assert.equal(stale.json?.settled, false, 'a final/stale epoch cannot settle twice');
    pass('dispatch transport fences stale duplicate settlement after the final outcome');
  } finally {
    await stopServer(server);
  }

  console.log('\nE2E PASS: the cross-node db-less worker HTTP surface (commit ingest + dispatch transport) works over the real server.');
}

main().catch((err) => {
  console.error('E2E FAIL:', err);
  process.exitCode = 1;
});
