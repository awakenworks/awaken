// Secretless model execution over the real cell + awaken-worker processes.
//
// TypeScript injects an opaque grant into a durable dispatch. A real database-less
// worker claims it over HTTP, passes it to its inference materializer, executes
// the returned model, commits through the claimed epoch, and settles the queue.
//
// Cause graph:
//   C0 request is an ordinary Run      -> E0 no Session realization control call
//   C1 opaque reference is authorized  -> E1 worker materializes without a raw key
//   C2 epoch/owner are current          -> E2 one committed provider result
//   C3 stdin reaches EOF in E2E mode    -> E3 graceful drain + coverage flush
//   C4 seed claim commits exact Ended   -> E4 durable receipt precedes Done removal
//
// Decision table:
//   Rule  C0  C1  C2  C3  Expected
//   T1    Y   Y   Y   N   E0 + E1 + E2; worker remains live
//   T2    Y   Y   Y   Y   E0 + E1 + E2 + E3
//   T3    Y   N   -   Y   fail closed; E3
//   T4    N   any any any claimed Session control is mandatory and fail-closed
//   T5    seed current owner/epoch commits Ended before Done -> E4

import assert from 'node:assert/strict';
import { spawn, type ChildProcessWithoutNullStreams } from 'node:child_process';
import fs, { mkdtempSync } from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { spawnServer, stopServer, waitForPort } from './harness.mjs';
import { cargoExecutable } from './cargo_binary.mjs';
import {
  nativeProviderCandidateFixture,
  providerCredentialTargetFixture,
} from './fixtures/provider_candidate_fixture.mjs';
import { ordinaryRunDispatchFixture } from './fixtures/run_dispatch_fixture.mjs';
import {
  claimedCommitRequestFixture,
  terminalThreadCommitFixture,
} from './fixtures/thread_commit_fixture.mjs';
import { workdirWorkerManifestFixture } from './fixtures/worker_manifest_fixture.mjs';

const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const PORT = Number(process.env.E2E_PORT ?? 38813);
// The stage orchestrator allocates this independently from the Control port so
// concurrent scenarios never alias the Worker's operational listener.
const WORKER_ADMIN_PORT = Number(process.env.E2E_WORKER_PORT ?? 38814);
const BASE = `http://127.0.0.1:${PORT}`;
const GRANT = 'grant-ts-provider-23';
const GRANT_REVISION = 1;
const THREAD = 'secretless-gateway-worker';
const REVALIDATION_THREAD = 'secretless-gateway-worker-revalidation';

function buildGatewayWorker(): string {
  return cargoExecutable({
    cwd: ROOT,
    packageName: 'awaken-cli',
    targetName: 'credential_reference_worker',
    targetKind: 'example',
  });
}

async function post(pathname: string, body: unknown, worker?: string): Promise<any> {
  const headers: Record<string, string> = { 'content-type': 'application/json' };
  if (worker) headers['x-awaken-worker-id'] = worker;
  const response = await fetch(`${BASE}${pathname}`, {
    method: 'POST',
    headers,
    body: JSON.stringify(body),
  });
  const text = await response.text();
  assert.equal(response.status, 200, `${pathname} accepted: ${text}`);
  return text ? JSON.parse(text) : {};
}

async function registerReadyWorker(workerId: string): Promise<any> {
  const incarnationId = `${workerId}-${process.pid}`;
  const registered = await post(
    '/v1/worker/register',
    {
      registration: {
        worker_id: workerId,
        incarnation_id: incarnationId,
        manifest: workdirWorkerManifestFixture({
          buildDigest: 'secretless-gateway-e2e',
          capabilities: ['host-executor/v1', 'native-runtime', 'session-resources/v1'],
        }),
      },
    },
    workerId,
  );
  const identity = registered.worker?.snapshot?.identity;
  assert.ok(identity, 'seed worker registration returned a durable incarnation identity');
  const heartbeat = await post(
    '/v1/worker/heartbeat',
    { identity, heartbeat: { sequence: 1, ready: true, in_flight: 0 } },
    workerId,
  );
  assert.equal(heartbeat.mutation, 'applied', 'seed worker entered the ready state');
  return identity;
}

async function waitForGatewayReply(timeoutMs = 30_000): Promise<any[]> {
  const deadline = Date.now() + timeoutMs;
  let observed: any[] = [];
  while (Date.now() <= deadline) {
    const response = await fetch(`${BASE}/v1/durable/threads/${THREAD}/messages`);
    if (response.status === 200) {
      observed = ((await response.json()) as any).messages ?? [];
      if (observed.some((message) => String(message.text ?? '').includes(`credential-reference:${GRANT}`))) {
        return observed;
      }
    }
    await new Promise((resolve) => setTimeout(resolve, 50));
  }
  throw new Error(`gateway worker never committed its grant-routed reply: ${JSON.stringify(observed)}`);
}

async function waitForOutput(
  output: () => string,
  expected: string,
  timeoutMs = 15_000,
): Promise<void> {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() <= deadline) {
    if (output().includes(expected)) return;
    await new Promise((resolve) => setTimeout(resolve, 50));
  }
  throw new Error(`worker output never contained ${expected}:\n${output()}`);
}

async function waitForDispatchSettlement(runId: string, timeoutMs = 30_000): Promise<void> {
  // Cause/effect graph: C1=model result commit is visible; C2=the Worker has
  // completed the following dispatch settle; E1=the exact Run disappears from
  // dispatch observation. Commit causally precedes settle but is not atomic with
  // it, so C1 alone must never be used as evidence for E1.
  //
  // | Rule | C1 reply visible | C2 settle complete | expected observation |
  // | T1   | no               | no                 | Run may be leased     |
  // | T2   | yes              | no                 | Run still present     |
  // | T3   | yes              | yes                | exact Run absent      |
  const deadline = Date.now() + timeoutMs;
  let observed: any[] = [];
  while (Date.now() <= deadline) {
    const response = await fetch(`${BASE}/v1/durable/threads/${THREAD}/dispatches`);
    assert.equal(response.status, 200);
    observed = ((await response.json()) as any).dispatches ?? [];
    if (!observed.some((dispatch) => dispatch.run_id === runId)) return;
    await new Promise((resolve) => setTimeout(resolve, 50));
  }
  throw new Error(`gateway dispatch did not settle: ${JSON.stringify(observed)}`);
}

async function main(): Promise<void> {
  const storage = mkdtempSync(path.join(tmpdir(), 'awaken-secretless-worker-'));
  const credentialState = path.join(storage, 'worker-local-credential-state');
  fs.writeFileSync(credentialState, 'available\n');
  const cell = spawnServer('echo', PORT, {
    SESSION_DEPLOYMENT_INGRESS: 'durable',
    SESSION_DEPLOYMENT_STORAGE_DIR: storage,
    SESSION_DEPLOYMENT_DISABLE_LOCAL_POOL: '1',
  }).server;
  let worker: ChildProcessWithoutNullStreams | undefined;
  let workerOutput = '';
  try {
    await waitForPort(PORT);

    // Obtain a real serialized activation from the server, then enqueue a second
    // stable dispatch carrying only an opaque gateway capability reference.
    const seedIdentity = await registerReadyWorker('seed-worker');
    await post(`/v1/durable/threads/${THREAD}-seed/submit_background`, { text: 'seed activation' });
    const seed = (
      await post('/v1/worker/dispatch/claim', { identity: seedIdentity }, 'seed-worker')
    ).claimed;
    assert.ok(seed, 'seed worker claimed the server-created activation');
    const gatewayRunId = `${seed.request.activation.run_id}-gateway`;
    const request = ordinaryRunDispatchFixture(seed.request, gatewayRunId, THREAD);
    // This hand-built dispatch is deliberately an ordinary Run. A non-null
    // session_thread_id is a claimed Session-control contract, not a history
    // grouping alias; activation.thread_id already owns committed history.
    // Raw Provider prerequisite: C-1 all required route coordinates, including
    // the opaque fixture dialect, are explicit -> E-1 typed ingress admits this
    // ordinary Run. Constraint/K: the shared fixture supplies no defaults or
    // validation; the Rust candidate deserializer remains the sole authority.
    // Decision rule R0=C-1=>E-1; malformed-coordinate rejection is owned once by
    // worker_transport plus the runtime-contract invariant test.
    request.activation.snapshot.resolved_spec.model_binding =
      nativeProviderCandidateFixture({
        binding: request.activation.snapshot.resolved_spec.model_binding,
        providerRef: 'fixture-provider@1',
        routeRef: 'fixture-worker-local@1',
        accessKind: 'direct',
        scopeId: 'fixture-workspace',
        credential: {
          credential: { id: GRANT, revision: GRANT_REVISION },
          material_source: 'worker_reference',
          target: providerCredentialTargetFixture('fixture-provider'),
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
        baseUrl: 'https://worker-local.invalid',
        upstreamModel: request.activation.snapshot.resolved_spec.model_binding.model_ref,
      });
    request.activation.snapshot.resolved_spec.model_candidates = [];
    request.inference_plaintext_holder = {
      boundary: 'worker', trust_domain: 'awaken.worker',
    };
    request.placement.required_capabilities = ['worker-local-credentials/v1', 'native-runtime'];
    request.placement.required_credentials = [{ id: GRANT, revision: GRANT_REVISION }];
    await post('/v1/worker/dispatch/enqueue', { request }, 'seed-worker');

    // T5/E4: enqueueing the cloned gateway Run is not committed truth for the
    // seed Run. The exact claim must first publish one durable terminal receipt;
    // missing/mismatched/nonterminal recovery is owned by the Rust settlement
    // decision table rather than duplicated in this positive E2E.
    const seedCommit = claimedCommitRequestFixture({
      claimed: seed,
      commit: terminalThreadCommitFixture({
        runId: seed.lease.run_id,
        threadId: seed.request.activation.thread_id,
        messageId: `seed-terminal-${seed.lease.run_id}`,
        text: 'seed ownership completed before gateway handoff',
      }),
      ordinal: 0,
      expectedThreadVersion: 0,
    });
    const seedCommitted = await post(
      '/v1/worker/commit-claimed',
      { ...seedCommit, identity: seedIdentity },
      'seed-worker',
    );
    assert.ok(
      typeof seedCommitted.commit_sequence === 'number',
      'T5/E4 durable seed receipt precedes Done settlement',
    );
    const seedSettle = await post(
      '/v1/worker/dispatch/settle',
      {
        run_id: seed.lease.run_id,
        epoch: seed.lease.epoch,
        outcome: 'Done',
        consumed: [],
        identity: seedIdentity,
      },
      'seed-worker',
    );
    assert.equal(seedSettle.settled, true, 'seed dispatch settled before the gateway worker starts');

    const env = { ...process.env } as Record<string, string>;
    Object.assign(env, {
      AWAKEN_UPSTREAM_URL: BASE,
      SESSION_DEPLOYMENT_INGRESS: 'durable',
      AWAKEN_WORKER_GATEWAY_ONLY: '1',
      AWAKEN_TEST_CREDENTIAL_ID: GRANT,
      AWAKEN_TEST_CREDENTIAL_REVISION: String(GRANT_REVISION),
      AWAKEN_TEST_CREDENTIAL_STATE_FILE: credentialState,
      AWAKEN_WORKER_ID: 'gateway-worker-ts',
      AWAKEN_WORKER_ADMIN_LISTEN: `127.0.0.1:${WORKER_ADMIN_PORT}`,
      AWAKEN_E2E_SHUTDOWN_ON_STDIN_EOF: '1',
      // This scenario verifies credential grant routing, not host namespace
      // availability. Keep the production fail-closed Namespace default and
      // make the test's unsafe local execution choice explicit.
      AWAKEN_TEST_SANDBOX_TIER: 'local',
    });
    worker = spawn(buildGatewayWorker(), [], { cwd: ROOT, env, stdio: ['pipe', 'pipe', 'pipe'] });
    worker.stdout.on('data', (chunk) => (workerOutput += chunk.toString()));
    worker.stderr.on('data', (chunk) => (workerOutput += chunk.toString()));

    const messages = await waitForGatewayReply().catch((error) => {
      return fetch(`${BASE}/v1/durable/threads/${THREAD}/dispatches`)
        .then(async (response) => {
          const dispatches = await response.text();
          throw new Error(
            `${error instanceof Error ? error.message : error}\ndispatches: ${dispatches}\nworker output:\n${workerOutput}`,
          );
        });
    });
    assert.equal(
      messages.filter((message) => String(message.text ?? '').includes(`credential-reference:${GRANT}`)).length,
      1,
      'the provider-routed model result committed exactly once',
    );
    assert.ok(!workerOutput.includes('provider-key'), 'worker output contains no provider credential');
    await waitForDispatchSettlement(gatewayRunId);

    // Placement still sees a fresh Available observation, while the exact
    // use-time provider check reports logout. The attempt must stop before the
    // materializer/Agent executor is entered.
    fs.writeFileSync(credentialState, 'available_then_login_required\n');
    const rejected = structuredClone(request);
    rejected.activation.run_id = `${request.activation.run_id}-revalidation`;
    rejected.activation.thread_id = REVALIDATION_THREAD;
    await post('/v1/worker/dispatch/enqueue', { request: rejected }, 'seed-worker');
    await waitForOutput(
      () => workerOutput,
      `worker-local credential ${GRANT} revision ${GRANT_REVISION} failed use-time revalidation`,
    );
    const rejectedMessages = await fetch(
      `${BASE}/v1/durable/threads/${REVALIDATION_THREAD}/messages`,
    );
    if (rejectedMessages.status === 200) {
      const body = ((await rejectedMessages.json()) as any).messages ?? [];
      assert.ok(
        !body.some((message: any) => String(message.text ?? '').includes(`credential-reference:${GRANT}`)),
        'logout after placement must not reach the model executor',
      );
    }

    console.log(
      'CREDENTIAL REFERENCE WORKER TS E2E PASS: fresh opaque state executed once; logout after placement failed exact use-time revalidation before Agent launch.',
    );
  } finally {
    if (worker) await stopServer(worker).catch(() => {});
    await stopServer(cell).catch(() => {});
    fs.rmSync(storage, { recursive: true, force: true });
  }
}

main().catch((error) => {
  console.error('SECRETLESS GATEWAY WORKER TS E2E FAIL:', error);
  process.exitCode = 1;
});
