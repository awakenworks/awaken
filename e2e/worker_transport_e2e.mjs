// The cross-node worker HTTP surface, end to end over the real server binary.
//
// A database-less worker drives runs and commits facts over HTTP — never opening
// the store. This exercises the two worker-facing seams the server now mounts:
//   - commit ingest  (POST /v1/worker/commit-claimed): the Worker pushes a
//     claim-fenced ThreadCommit,
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
import {
  WORKER_PROVIDER_CREDENTIAL_CAPABILITY,
  pass,
  spawnServer,
  stopServer,
  waitForPort,
} from './harness.mjs';
import { nativeProviderCandidateFixture } from './fixtures/provider_candidate_fixture.mjs';
import {
  claimedCommitRequestFixture,
  terminalThreadCommitFixture,
} from './fixtures/thread_commit_fixture.mjs';

const PORT = Number(process.env.E2E_PORT ?? 38812);
const BASE = `http://127.0.0.1:${PORT}`;
const THREAD = 'worker-transport-1';
const ENV = {
  SESSION_DEPLOYMENT_INGRESS: 'durable',
  SESSION_DEPLOYMENT_STORAGE_DIR: mkdtempSync(path.join(tmpdir(), 'awaken-worker-transport-')),
  // Keep queued work available for this external worker instead of racing the
  // scenario host's in-process drain pool.
  SESSION_DEPLOYMENT_DISABLE_LOCAL_POOL: '1',
};
const WORKER = 'ts-worker-1';
let workerIdentity;

function claimedCommitRequest(claimed, text) {
  const runId = claimed.lease.run_id;
  return claimedCommitRequestFixture({
    claimed,
    commit: terminalThreadCommitFixture({
      runId,
      threadId: claimed.request.activation.thread_id,
      messageId: `a-${runId}`,
      text,
    }),
    ordinal: 0,
    expectedThreadVersion: 0,
  });
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

async function postArtifact(claim, logicalPath) {
  // The empty-input BLAKE3 vector is pinned by awaken-resource-contract. Using
  // it here avoids introducing a second JavaScript digest implementation while
  // still proving that the real HTTP adapter verifies the canonical content id.
  // These effect ids are golden outputs of the authoritative Rust
  // `harvest_idempotency_key(thread, path, content_id)` codec; keeping only its
  // outputs here avoids a parallel JavaScript implementation of that protocol.
  const effectIds = {
    'reports/worker-result.bin': 'ee88fe25e1e8813bec179b2037aeb40f0c9958db2faa664501e558cd911a234d',
    'reports/late-result.bin': '5e5503f53755eade26e8bb686ad23165bcdcea9b2c97e4094f5befbe755e13b3',
  };
  assert.equal(claim.request.session_thread_id, 'worker-dispatch-auth-1-grant');
  assert.ok(effectIds[logicalPath], `missing canonical artifact effect fixture for ${logicalPath}`);
  const metadata = {
    claim: {
      run_id: claim.lease.run_id,
      owner: claim.lease.owner,
      epoch: claim.lease.epoch,
    },
    identity: workerIdentity,
    workspace_id: claim.request.execution_scope,
    session_id: claim.request.session_thread_id,
    logical_path: logicalPath,
    mime_type: 'application/octet-stream',
    content_id: 'af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262',
    effect_id: effectIds[logicalPath],
  };
  const response = await fetch(`${BASE}/v1/worker/resources/files/artifacts`, {
    method: 'POST',
    headers: {
      'content-type': 'application/octet-stream',
      'x-awaken-worker-id': WORKER,
      'x-awaken-artifact-publication': Buffer.from(JSON.stringify(metadata)).toString('base64url'),
    },
    body: Buffer.alloc(0),
  });
  const text = await response.text();
  let json;
  try {
    json = JSON.parse(text);
  } catch {
    json = null;
  }
  return { status: response.status, json, text };
}

async function registerReadyWorker() {
  const registration = await postJson('/v1/worker/register', {
    registration: {
      worker_id: WORKER,
      incarnation_id: `${WORKER}-${process.pid}`,
      manifest: {
        manifest_version: 1,
        build_digest: 'worker-transport-e2e',
        capabilities: [
          'credential-source/v1',
          'host-executor/v1',
          'native-runtime',
          WORKER_PROVIDER_CREDENTIAL_CAPABILITY,
        ],
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
    await waitForPort(PORT, 180_000, server);

    // Every worker route is authenticated. The local composition uses the
    // compatibility identity header; cloud replaces the authenticator with a
    // WorkerLease/mTLS implementation behind the same port.
    const anonymous = await postJson('/v1/worker/commit-claimed', {}, null);
    assert.equal(anonymous.status, 401, `anonymous worker is rejected: ${anonymous.text}`);
    pass('worker transport rejects a request with no authenticated identity');
    await registerReadyWorker();

    // Session-realization authority cause graph:
    // C1 authenticated identity is current
    //   -> C2 command is renewal-only
    //   -> C3 owner + Runtime incarnation are exact
    //   -> C4 requested expiry is live and bounded by the registry lease
    //   -> E1 invoke the one SessionRealizationControl port.
    // Any failed cause stops before Session-domain lookup or effects.
    //
    // | Rule | renew | owner/incarnation | expiry | Result |
    // | S1 | false | exact | bounded | reject at C2 |
    // | S2 | true | wrong owner | bounded | reject at C3 |
    // | S3 | true | wrong incarnation | bounded | reject at C3 |
    // | S4 | true | exact | expired | reject at C4 |
    // | S5 | true | exact | beyond registry | reject at C4 |
    // | S6 | true | exact | bounded | reaches control; 404 `not_found` |
    // | S7 | true | exact | bounded | preparing Session; 409 `not_ready` |
    const incarnation = `${workerIdentity.worker_id}:${workerIdentity.generation}:${workerIdentity.incarnation_id}`;
    const boundedExpiry = Date.now() + 5_000;
    const beginCommand = (overrides = {}) => ({
      session_id: 'sesn_worker_authority_probe',
      target: {
        owner: workerIdentity.worker_id,
        runtime_incarnation: incarnation,
        lease_expires_at_unix_ms: boundedExpiry,
        renew_existing_lease: true,
        ...overrides,
      },
    });
    const authorityCases = [
      ['S1', { renew_existing_lease: false }],
      ['S2', { owner: 'another-worker' }],
      ['S3', { runtime_incarnation: 'another-incarnation' }],
      ['S4', { lease_expires_at_unix_ms: 1 }],
      ['S5', { lease_expires_at_unix_ms: Date.now() + 86_400_000 }],
    ];
    for (const [rule, override] of authorityCases) {
      const response = await postJson('/v1/worker/session/realization/begin', {
        command: beginCommand(override),
      });
      assert.equal(response.status, 400, `${rule}: ${response.text}`);
      assert.match(response.text, /renewal exceeds authenticated Worker authority/u, rule);
    }
    const admitted = await postJson('/v1/worker/session/realization/begin', {
      command: beginCommand(),
    });
    assert.equal(admitted.status, 404, `S6: ${admitted.text}`);
    assert.equal(admitted.json?.realization_error?.kind, 'not_found', 'S6 typed control result');
    assert.doesNotMatch(
      admitted.text,
      /renewal exceeds authenticated Worker authority/u,
      'S6 passed the transport fence and reached the sole Session control port',
    );

    const wrongLease = {
      owner: 'another-worker',
      runtime_incarnation: incarnation,
      epoch: 1,
      expires_at_unix_ms: boundedExpiry,
    };
    for (const route of ['activate', 'acknowledge', 'fail']) {
      const command = route === 'activate'
        ? { session_id: 'sesn_worker_authority_probe', lease: wrongLease, mcp_receipts: [] }
        : route === 'acknowledge'
          ? { session_id: 'sesn_worker_authority_probe', lease: wrongLease, published: [], drained: [] }
          : { session_id: 'sesn_worker_authority_probe', lease: wrongLease, reason: 'probe' };
      const response = await postJson(`/v1/worker/session/realization/${route}`, { command });
      assert.equal(response.status, 400, `${route}: ${response.text}`);
      assert.match(response.text, /lease is not owned by the authenticated Worker incarnation/u);
    }
    pass('Session realization transport enforces renewal, owner, incarnation, and registry expiry');

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

    // Cause-effect graph for authenticated credential-bearing transport:
    // C1 current Worker identity + C2 exact realization capability + C3 allowed holder
    //   -> E1 claim persists the binding and returns the immutable candidate.
    // C1 + (!C2 || !C3) -> E2 skip/reject before credential materialization.
    // C4 claim owner/epoch current + C5 canonical operation/hash -> E3 commit once.
    // !C4 -> E4 unauthorized/stale; repeated C5 -> E5 duplicate receipt, no second fact.
    // C6 the exact Run is durably Ended -> E6 terminal observation precedes Done settlement.
    // !C6 -> E7 reject Done and retain the claim; the negative partition is owned by
    // the settlement adapter's Rust decision table rather than duplicated here.
    //
    // | Rule | Identity | Capability/holder | Claim | Operation | Result          |
    // | T1   | current  | admitted          | -     | -         | exact claim     |
    // | T2   | current  | unsupported       | -     | -         | no claim        |
    // | T3   | wrong    | admitted          | exact | valid     | unauthorized    |
    // | T4   | current  | admitted          | exact | valid     | commit once     |
    // | T5   | current  | admitted          | exact | replay    | duplicate receipt|
    // | T6   | current  | admitted          | exact | Ended     | settle Done       |
    // Re-enqueue the exact durable wire record with one complete, immutable model
    // candidate. Dispatch persists/transports it without interpreting the route or
    // carrying provider key material.
    const granted = structuredClone(claimed.request);
    granted.activation.run_id = `${claimed.request.activation.run_id}-grant`;
    granted.activation.thread_id = `${claimed.request.activation.thread_id}-grant`;
    granted.session_thread_id = granted.activation.thread_id;
    granted.execution_scope = 'scope-ts-17';
    const pinnedCandidate = nativeProviderCandidateFixture({
      binding: granted.activation.snapshot.resolved_spec.model_binding,
      providerRef: 'fixture-provider@1',
      routeRef: 'fixture-route@1',
      scopeId: 'scope-ts-17',
      credential: {
        credential: { id: 'grant-ts-17', revision: 3 },
        material_source: 'control_plane_reference',
        usage: { type: 'provider_adapter' },
        policy: {
          allowed_plaintext_holders: [
            { boundary: 'worker', trust_domain: 'awaken.worker' },
          ],
          model_exposure: 'forbidden',
        },
      },
      adapterKind: 'fixture',
      apiDialect: 'fixture',
      baseUrl: 'https://fixture.invalid/v1',
      upstreamModel: granted.activation.snapshot.resolved_spec.model_binding.model_ref,
    });

    // Typed Provider-ingress test design. Causes: C0 every required Provider
    // coordinate is explicit; C6 one required endpoint coordinate is empty.
    // Effects: E0 C0 reaches the queue unchanged; E6 C6 is rejected with 422
    // and creates no dispatch row. Constraint/invariant: the fixture supplies
    // no defaults or validation; ResolvedModelCandidate deserialization is the
    // only validity authority. Decision rules: W1=C0=>E0; W2=C6=>E6.
    const malformedRoute = structuredClone(granted);
    malformedRoute.activation.run_id = `${granted.activation.run_id}-malformed-route`;
    malformedRoute.activation.thread_id = `${granted.activation.thread_id}-malformed-route`;
    malformedRoute.session_thread_id = malformedRoute.activation.thread_id;
    malformedRoute.activation.snapshot.resolved_spec.model_binding =
      structuredClone(pinnedCandidate);
    malformedRoute.activation.snapshot.resolved_spec.model_binding
      .provisioning.endpoint.upstream_model = '';
    malformedRoute.activation.snapshot.resolved_spec.model_candidates = [];
    const rejectedRoute = await postJson('/v1/worker/dispatch/enqueue', {
      request: malformedRoute,
    });
    assert.equal(rejectedRoute.status, 422, `W2 typed rejection: ${rejectedRoute.text}`);
    assert.match(
      rejectedRoute.text,
      /provider provisioning requires complete canonical route coordinates/u,
      'W2 reports the closed Provider-coordinate invariant',
    );
    const malformedQueue = await fetch(
      `${BASE}/v1/durable/threads/${malformedRoute.activation.thread_id}/dispatches`,
    );
    const malformedQueueText = await malformedQueue.text();
    assert.equal(malformedQueue.status, 200, `W2 queue observation: ${malformedQueueText}`);
    assert.deepEqual(
      JSON.parse(malformedQueueText).dispatches ?? [],
      [],
      'W2 malformed Provider bytes never mutate the durable queue',
    );
    pass('typed dispatch ingress rejects an incomplete Provider route before queue mutation');

    granted.activation.snapshot.resolved_spec.model_binding = pinnedCandidate;
    granted.activation.snapshot.resolved_spec.model_candidates = [];
    granted.inference_plaintext_holder = {
      boundary: 'worker', trust_domain: 'awaken.worker',
    };
    granted.placement.required_capabilities = ['credential-source/v1', 'native-runtime'];
    const enqueuedGrant = await postJson('/v1/worker/dispatch/enqueue', { request: granted });
    assert.equal(enqueuedGrant.status, 200, `grant-bearing dispatch enqueued: ${enqueuedGrant.text}`);

    // Complete the first ownership before claiming the next run. Done is not a
    // Worker assertion: the guarded Coordinator first reads the exact committed
    // terminal truth, performs terminal observation, and only then settles.
    const initialCommit = claimedCommitRequest(claimed, 'first claimed terminal commit');
    const initialCommitted = await postJson('/v1/worker/commit-claimed', initialCommit);
    assert.equal(
      initialCommitted.status,
      200,
      `first claimed Run commits before settlement: ${initialCommitted.text}`,
    );
    assert.ok(
      typeof initialCommitted.json?.commit_sequence === 'number',
      'first terminal commit returns the durable receipt',
    );
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
    assert.ok(grant, `credential-compatible Worker claims the exact dispatch: ${grantClaim.text}`);
    assert.deepEqual(
      grant?.request?.activation?.snapshot?.resolved_spec?.model_binding,
      pinnedCandidate,
      'the complete published candidate survives enqueue → durable store → authenticated claim unchanged',
    );
    assert.equal(
      grant?.request?.execution_scope,
      'scope-ts-17',
      'verified execution scope survives the durable worker boundary as an opaque coordinate',
    );
    assert.ok(!JSON.stringify(grant).includes('provider-key'), 'claim contains no provider credential');
    pass('secret-free published model candidate survives durable dispatch without a provider key');

    // Remote artifact-publication FMECA / cause-effect decision table:
    // C1 the registered incarnation is authenticated; C2 the claim owner/epoch
    // is live; C3 Workspace and Session equal the frozen dispatch; C4 the body
    // matches the canonical content id. E1 persists one downloadable scoped File;
    // E2 an identical at-least-once delivery returns the same File; E3 a final
    // claim rejects every late publication before the Resource application.
    //
    // | Rule | C1 | C2 | C3 | C4 | Effect |
    // | A1 | yes | live | exact | exact | E1 |
    // | A2 | yes | same live claim | exact | same | E2 |
    // | A3 | yes | settled | exact | exact | E3 (409) |
    //
    // Auth/incarnation, cross-scope, path, digest, and Resource-outage negative
    // partitions are owned by the same adapter's Rust A1-A12 table; duplicating
    // them here would add no process-boundary interaction. This scenario owns
    // the live/final claim transition over the real server binary.
    const artifact = await postArtifact(grant, 'reports/worker-result.bin');
    assert.equal(artifact.status, 200, `live claim publishes an artifact: ${artifact.text}`);
    assert.equal(artifact.json?.record?.scope_id, grant.request.session_thread_id);
    assert.equal(artifact.json?.record?.logical_path, 'reports/worker-result.bin');
    assert.equal(artifact.json?.record?.downloadable, true);
    assert.equal(artifact.json?.effect_id, 'ee88fe25e1e8813bec179b2037aeb40f0c9958db2faa664501e558cd911a234d');
    const artifactReplay = await postArtifact(grant, 'reports/worker-result.bin');
    assert.equal(artifactReplay.status, 200, `artifact replay is accepted: ${artifactReplay.text}`);
    assert.equal(
      artifactReplay.json?.record?.id,
      artifact.json?.record?.id,
      'same claim/path/bytes has one File effect',
    );
    pass('live remote claim publishes one idempotent claim-fenced File');

    // A different authenticated worker cannot commit the claim. The owner-bound
    // request is rejected before thread facts are applied.
    const claimedCommit = claimedCommitRequest(
      grant,
      'claimed commit from authenticated worker',
    );
    const wrongOwner = await postJson(
      '/v1/worker/commit-claimed',
      { ...claimedCommit, identity: workerIdentity },
      'worker-thief',
    );
    assert.equal(wrongOwner.status, 401, `wrong claim owner rejected: ${wrongOwner.text}`);
    const committedClaim = await postJson('/v1/worker/commit-claimed', claimedCommit);
    assert.equal(committedClaim.status, 200, `current owner/epoch commits: ${committedClaim.text}`);
    assert.ok(typeof committedClaim.json?.commit_sequence === 'number');
    const replay = await postJson('/v1/worker/commit-claimed', claimedCommit);
    assert.equal(replay.status, 200, `claimed commit redelivery accepted: ${replay.text}`);
    assert.equal(replay.json?.duplicate, true, 'the stable operation id returns a duplicate receipt');
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
    const lateArtifact = await postArtifact(grant, 'reports/late-result.bin');
    assert.equal(lateArtifact.status, 409, `settled claim rejects a late artifact: ${lateArtifact.text}`);
    const stale = await postJson('/v1/worker/dispatch/settle', {
      run_id: grant.lease.run_id,
      epoch: grant.lease.epoch,
      outcome: 'Done',
      consumed: [],
    });
    assert.equal(stale.json?.settled, false, 'a final/stale epoch cannot settle twice');
    pass('dispatch transport fences stale settlement and late artifact publication after the final outcome');
  } finally {
    await stopServer(server);
  }

  console.log('\nE2E PASS: the cross-node db-less worker HTTP surface (commit ingest + dispatch transport) works over the real server.');
}

main().catch((err) => {
  console.error('E2E FAIL:', err);
  process.exitCode = 1;
});
