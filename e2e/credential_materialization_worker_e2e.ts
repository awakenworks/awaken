// Production Worker process E2E: the real `awaken-worker` artifact owns no authority
// database or seal key. A recipient-bound CSI-style projection supplies only the
// exact credential material pinned by its dispatch; Resource reads use the same
// authenticated Worker upstream and the result commits through the claim fence.
//
// Cause/effect decision table:
// P0 ordinary dispatch + no Session pointer -> no realization-control lookup;
// P1 exact recipient + live expiry + payload marker + target + claim -> one
// provider call, one committed reply; P2 any changed envelope/target dimension
// -> no provider call; P3 no authority DB/seal configuration -> Worker starts
// and leaves no authority files; P4 structurally complete but stale or
// unauthorized publication pin -> fail closed while the same Worker remains
// available for P1; P5 the current seed owner commits its exact Run as Ended
// and receives a durable receipt before terminal observation and Done removal;
// a non-null Session
// pointer is constrained to a real Managed Session and fails closed otherwise.

import assert from 'node:assert/strict';
import { spawn, type ChildProcessWithoutNullStreams } from 'node:child_process';
import fs, { mkdtempSync } from 'node:fs';
import { request as httpRequest } from 'node:http';
import { createServer as createHttpsServer, type Server as HttpsServer } from 'node:https';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { deploymentEnv, spawnServer, stopServer, waitForPort } from './harness.mjs';
import { startFakeAnthropic } from './fixtures/fake_anthropic_fixture.mjs';
import { nativeProviderCandidateFixture } from './fixtures/provider_candidate_fixture.mjs';
import {
  claimedCommitRequestFixture,
  terminalThreadCommitFixture,
} from './fixtures/thread_commit_fixture.mjs';
import { createTlsIdentityFixture } from './fixtures/tls_identity_fixture.mjs';
// @ts-expect-error The shared Cargo artifact resolver is intentionally JavaScript.
import { WORKER_BIN_ENV, cargoExecutable } from './cargo_binary.mjs';

const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const PORT = Number(process.env.E2E_PORT ?? 38823);
const CONFIG_PORT = Number(process.env.E2E_CONFIG_PORT ?? 40823);
const TLS_PORT = Number(process.env.E2E_WORKER_PORT ?? 39824);
const BASE = `http://127.0.0.1:${PORT}`;
const CONFIG_BASE = `http://127.0.0.1:${CONFIG_PORT}`;
const WORKER_BASE = `https://127.0.0.1:${TLS_PORT}`;
const THREAD = 'credential-materialization-worker';
const SEAL_KEY = '1234567890abcdef1234567890abcdef1234567890abcdef1234567890abcdef';
const PROVIDER_KEY = 'sk-worker-materialization-e2e'; // awaken-allow: secret

function hexComponent(value: string): string {
  return Buffer.from(value).toString('hex');
}

function stableFingerprint(value: unknown): string {
  const bytes = Buffer.from(JSON.stringify(value));
  let hash = 0xcbf29ce484222325n;
  for (const byte of bytes) {
    hash ^= BigInt(byte);
    hash = BigInt.asUintN(64, hash * 0x100000001b3n);
  }
  return `fnv1a64:${hash.toString(16).padStart(16, '0')}`;
}

function workerBinary(): string {
  return cargoExecutable({
    cwd: ROOT,
    packageName: 'awaken-worker',
    targetName: 'awaken-worker',
    prebuiltEnvironmentName: WORKER_BIN_ENV,
  });
}

async function startTlsProxy(
  port: number,
  targetPort: number,
  certificate: string,
  key: string,
): Promise<HttpsServer> {
  const server = createHttpsServer(
    { cert: fs.readFileSync(certificate), key: fs.readFileSync(key) },
    (incoming, outgoing) => {
      const upstream = httpRequest({
        hostname: '127.0.0.1',
        port: targetPort,
        method: incoming.method,
        path: incoming.url,
        headers: incoming.headers,
      }, (response) => {
        outgoing.writeHead(response.statusCode ?? 502, response.headers);
        response.pipe(outgoing);
      });
      upstream.on('error', (error) => {
        if (!outgoing.headersSent) outgoing.writeHead(502);
        outgoing.end(`TLS proxy upstream failed: ${error.message}`);
      });
      incoming.pipe(upstream);
    },
  );
  await new Promise<void>((resolve, reject) => {
    server.once('error', reject);
    server.listen(port, '127.0.0.1', () => {
      server.off('error', reject);
      resolve();
    });
  });
  return server;
}

async function stopTlsProxy(server: HttpsServer): Promise<void> {
  await new Promise<void>((resolve, reject) => {
    server.close((error) => (error ? reject(error) : resolve()));
  });
}

async function request(
  method: string,
  pathname: string,
  body?: unknown,
  worker?: string,
  base = BASE,
) {
  const headers: Record<string, string> = {};
  if (body !== undefined) headers['content-type'] = 'application/json';
  if (worker) headers['x-awaken-worker-id'] = worker;
  const response = await fetch(`${base}${pathname}`, {
    method,
    headers,
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  const text = await response.text();
  let json: any = null;
  try { json = text ? JSON.parse(text) : null; } catch { /* retain text for diagnostics */ }
  return { status: response.status, json, text };
}

async function seedIdentity() {
  const id = 'materialization-seed';
  const registered = await request('POST', '/v1/worker/register', {
    registration: {
      worker_id: id,
      incarnation_id: `${id}-${process.pid}`,
      manifest: {
        manifest_version: 1,
        build_digest: id,
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
  }, id);
  assert.equal(registered.status, 200, registered.text);
  const identity = registered.json.worker.snapshot.identity;
  const heartbeat = await request('POST', '/v1/worker/heartbeat', {
    identity,
    heartbeat: { sequence: 1, ready: true, in_flight: 0 },
  }, id);
  assert.equal(heartbeat.status, 200, heartbeat.text);
  return { id, identity };
}

async function waitForReply(timeoutMs = 30_000) {
  const deadline = Date.now() + timeoutMs;
  let messages: any[] = [];
  while (Date.now() <= deadline) {
    const response = await request('GET', `/v1/durable/threads/${THREAD}/messages`);
    if (response.status === 200) {
      messages = response.json.messages ?? [];
      if (messages.some((message: any) => String(message.text ?? '').includes('FAKE:seed activation'))) {
        return messages;
      }
    }
    await new Promise((resolve) => setTimeout(resolve, 50));
  }
  throw new Error(`provider reply was not committed: ${JSON.stringify(messages)}`);
}

async function main() {
  const storage = mkdtempSync(path.join(tmpdir(), 'awaken-materialization-worker-'));
  // TLS fixture rule T1: explicit IP SAN + the projected private CA lets the
  // production Worker reach only this HTTPS edge. T2: product admission and
  // certificate validation remain owned by WorkerUpstream/reqwest; this shared
  // fixture creates bytes but supplies no trust default or bypass.
  const tlsIdentity = createTlsIdentityFixture(storage, {
    caCommonName: 'Awaken E2E Worker CA',
    serverCommonName: '127.0.0.1',
    subjectAltNames: ['IP:127.0.0.1'],
  });
  const upstream = await startFakeAnthropic(PROVIDER_KEY);
  const management = spawnServer(
    'management',
    CONFIG_PORT,
    {
      ...deploymentEnv(storage, { identityMode: 'no-login', controlSealKey: SEAL_KEY }),
      // spawnServer runs the scenario composition, whose one explicit fixture
      // input is SESSION_DEPLOYMENT_STORAGE_DIR. HOME above configures the
      // management stores; this value configures its Coordinator runtime.
      SESSION_DEPLOYMENT_STORAGE_DIR: storage,
    },
  ).server;
  const cell = spawnServer('echo', PORT, {
    SESSION_DEPLOYMENT_INGRESS: 'durable',
    SESSION_DEPLOYMENT_STORAGE_DIR: storage,
    SESSION_DEPLOYMENT_DISABLE_LOCAL_POOL: '1',
  }).server;
  let worker: ChildProcessWithoutNullStreams | undefined;
  let tlsProxy: HttpsServer | undefined;
  let output = '';
  try {
    await waitForPort(CONFIG_PORT);
    await waitForPort(PORT);
    tlsProxy = await startTlsProxy(
      TLS_PORT,
      PORT,
      tlsIdentity.serverCertificate,
      tlsIdentity.serverKey,
    );
    const credential = await request('POST', '/v1/config/credentials', {
      workspace_id: 'client-scope-is-overridden',
      kind: 'vault',
      provider_id: 'anthropic',
      env_key: null,
      secret: PROVIDER_KEY,
    }, undefined, CONFIG_BASE);
    assert.equal(credential.status, 201, credential.text);

    const seed = await seedIdentity();
    assert.equal((await request(
      'POST', `/v1/durable/threads/${THREAD}-seed/submit_background`, { text: 'seed activation' },
    )).status, 200);
    const claim = await request(
      'POST', '/v1/worker/dispatch/claim', { identity: seed.identity }, seed.id,
    );
    assert.equal(claim.status, 200, claim.text);
    const claimed = claim.json.claimed;
    assert.ok(claimed, 'seed activation claimed');

    const dispatch = structuredClone(claimed.request);
    dispatch.activation.run_id = `${claimed.request.activation.run_id}-materialized`;
    dispatch.activation.thread_id = THREAD;
    // This fixture exercises an ordinary durable Run, not a Managed Session.
    // Its activation thread remains the commit/history boundary; inventing a
    // Session pointer would correctly require a corresponding Control record.
    dispatch.session_thread_id = null;
    // Raw Provider prerequisite: C-1 every required route coordinate is explicit
    // and the Anthropic dialect agrees with its adapter -> E-1 typed ingress can
    // enqueue the candidate for the materialization decision table below.
    // Constraint/K: the fixture has no defaults or validator; Rust candidate
    // deserialization owns structural validity. Rule P-1=C-1=>E-1; incomplete
    // route bytes are owned by worker_transport, not duplicated at Worker use.
    const publishedCandidate = nativeProviderCandidateFixture({
      binding: {
        ...structuredClone(dispatch.activation.snapshot.resolved_spec.model_binding),
        provider_identity_ref: 'anthropic',
        model_ref: 'fake-worker-model',
        backend_ref: 'genai',
      },
      providerRef: 'anthropic@1',
      routeRef: 'fake-endpoint@1',
      accessKind: 'direct',
      scopeId: credential.json.workspace_id,
      credential: {
        credential: { id: credential.json.id, revision: credential.json.version },
        material_source: 'control_plane_reference',
        envelope: {
          type: 'sealed_for_worker',
          envelope_ref: {
            id: 'materialization-envelope',
            payload_fingerprint: 'sha256:materialization-payload',
          },
          recipient: 'awaken.worker',
          expires_at_unix_ms: Date.now() + 120_000,
        },
        usage: { type: 'provider_adapter' },
        policy: {
          allowed_plaintext_holders: [
            { boundary: 'worker', trust_domain: 'awaken.worker' },
            { boundary: 'workload', trust_domain: 'awaken.workload.acp' },
          ],
          model_exposure: 'forbidden',
        },
      },
      adapterKind: 'anthropic',
      apiDialect: 'anthropic_messages',
      baseUrl: `${upstream.url}/v1/`,
      upstreamModel: 'fake-worker-model',
    });
    dispatch.activation.snapshot.resolved_spec.model_binding = publishedCandidate;
    dispatch.activation.snapshot.resolved_spec.model_candidates = [];
    dispatch.inference_plaintext_holder = {
      boundary: 'worker', trust_domain: 'awaken.worker',
    };
    dispatch.placement.required_capabilities = ['credential-source/v1', 'native-runtime'];

    // The same production Worker must fail closed for structurally complete but
    // stale or unauthorized pins and continue draining. Structurally incomplete
    // candidates stop at typed HTTP ingress in worker_transport; retaining that
    // row here would invent a second, unreachable Worker-level oracle. Dispatch
    // retry policy retains these admitted failures; none may reach a provider or
    // fall back to catalog resolution.
    const invalidCandidates = [
      {
        ...structuredClone(publishedCandidate),
        provisioning: {
          ...structuredClone(publishedCandidate.provisioning),
          credential: {
            ...structuredClone(publishedCandidate.provisioning.credential),
            envelope: {
              ...structuredClone(publishedCandidate.provisioning.credential.envelope),
              envelope_ref: {
                ...structuredClone(
                  publishedCandidate.provisioning.credential.envelope.envelope_ref,
                ),
                payload_fingerprint: 'sha256:substituted-payload',
              },
            },
          },
        },
      },
      {
        ...structuredClone(publishedCandidate),
        provisioning: {
          ...structuredClone(publishedCandidate.provisioning),
          credential: {
            ...structuredClone(publishedCandidate.provisioning.credential),
            envelope: {
              ...structuredClone(publishedCandidate.provisioning.credential.envelope),
              expires_at_unix_ms: 1,
            },
          },
        },
      },
      {
        ...structuredClone(publishedCandidate),
        provisioning: {
          ...structuredClone(publishedCandidate.provisioning),
          credential: {
            ...structuredClone(publishedCandidate.provisioning.credential),
            envelope: {
              ...structuredClone(publishedCandidate.provisioning.credential.envelope),
              recipient: 'another.worker',
            },
          },
        },
      },
      {
        ...structuredClone(publishedCandidate),
        provisioning: {
          ...structuredClone(publishedCandidate.provisioning),
          provider_ref: 'unversioned-provider',
        },
      },
      {
        ...structuredClone(publishedCandidate),
        provisioning: {
          ...structuredClone(publishedCandidate.provisioning),
          credential: {
            ...structuredClone(publishedCandidate.provisioning.credential),
          credential: { id: 'different-credential', revision: credential.json.version },
          },
        },
      },
      {
        ...structuredClone(publishedCandidate),
        provisioning: {
          ...structuredClone(publishedCandidate.provisioning),
          scope_id: 'foreign-workspace',
        },
      },
      {
        ...structuredClone(publishedCandidate),
        provisioning: {
          ...structuredClone(publishedCandidate.provisioning),
          credential: {
            credential: structuredClone(
              publishedCandidate.provisioning.credential.credential,
            ),
            injection: 'direct',
            usage: structuredClone(publishedCandidate.provisioning.credential.usage),
            policy: structuredClone(publishedCandidate.provisioning.credential.policy),
          },
        },
      },
    ];
    for (const [index, candidate] of invalidCandidates.entries()) {
      candidate.provisioning.endpoint.upstream_model = `invalid-${index}`;
      const invalid = structuredClone(dispatch);
      const thread = `${THREAD}-invalid-${index}`;
      invalid.activation.run_id = `${claimed.request.activation.run_id}-invalid-${index}`;
      invalid.activation.thread_id = thread;
      invalid.session_thread_id = null;
      invalid.activation.snapshot.resolved_spec.model_binding = candidate;
      const invalidEnqueue = await request(
        'POST', '/v1/worker/dispatch/enqueue', { request: invalid }, seed.id,
      );
      assert.equal(invalidEnqueue.status, 200, invalidEnqueue.text);
    }
    assert.equal((await request('POST', '/v1/worker/dispatch/enqueue', { request: dispatch }, seed.id)).status, 200);

    // P5: enqueueing the cloned materialization Run does not commit the seed
    // Run. Preserve the canonical commit -> durable receipt -> Done ordering;
    // negative recovery partitions remain owned by the Rust settlement table.
    const seedCommit = claimedCommitRequestFixture({
      claimed,
      commit: terminalThreadCommitFixture({
        runId: claimed.lease.run_id,
        threadId: claimed.request.activation.thread_id,
        messageId: `seed-terminal-${claimed.lease.run_id}`,
        text: 'seed ownership completed before credential materialization',
      }),
      ordinal: 0,
      expectedThreadVersion: 0,
    });
    const seedCommitted = await request(
      'POST',
      '/v1/worker/commit-claimed',
      { ...seedCommit, identity: seed.identity },
      seed.id,
    );
    assert.equal(seedCommitted.status, 200, seedCommitted.text);
    assert.ok(
      typeof seedCommitted.json.commit_sequence === 'number',
      'P5 durable seed receipt precedes Done settlement',
    );
    const settled = await request('POST', '/v1/worker/dispatch/settle', {
      run_id: claimed.lease.run_id,
      epoch: claimed.lease.epoch,
      outcome: 'Done',
      consumed: [],
      identity: seed.identity,
    }, seed.id);
    assert.equal(settled.json.settled, true, settled.text);

    const materialRoot = path.join(storage, 'projected-worker-credentials');
    const targetUseFingerprint = stableFingerprint([
      [publishedCandidate.provisioning.provider_ref, publishedCandidate.provisioning.endpoint],
      publishedCandidate.provisioning.credential.usage,
    ]);
    const materialDirectory = path.join(
      materialRoot,
      'envelopes',
      hexComponent(publishedCandidate.provisioning.credential.envelope.envelope_ref.id),
      hexComponent(
        publishedCandidate.provisioning.credential.envelope.envelope_ref.payload_fingerprint,
      ),
      hexComponent(credential.json.id),
      String(credential.json.version),
      hexComponent(credential.json.workspace_id),
      hexComponent(targetUseFingerprint),
    );
    fs.mkdirSync(materialDirectory, { recursive: true });
    fs.writeFileSync(
      path.join(materialDirectory, 'payload_fingerprint'),
      `${publishedCandidate.provisioning.credential.envelope.envelope_ref.payload_fingerprint}\n`,
    );
    fs.writeFileSync(path.join(materialDirectory, 'secret'), PROVIDER_KEY);

    const workerConfig = path.join(storage, 'worker.toml');
    const workerRequestCredential = path.join(storage, 'worker-request-credential.json');
    fs.writeFileSync(workerRequestCredential, JSON.stringify({
      worker_id: 'materialization-worker',
      key_id: 'materialization-key',
      credential_id: 'materialization-request-credential',
      secret_base64: Buffer.from('materialization-worker-request-secret').toString('base64'),
    }));
    fs.writeFileSync(workerConfig, [
      `data_dir = ${JSON.stringify(path.join(storage, 'worker'))}`,
      'worker_id = "materialization-worker"',
      `worker_request_credential_file = ${JSON.stringify(workerRequestCredential)}`,
      `worker_server_ca_certificate_file = ${JSON.stringify(tlsIdentity.caCertificate)}`,
      `worker_credential_material_root = ${JSON.stringify(materialRoot)}`,
      'worker_admin_listen = "127.0.0.1:39823"',
      // Credential envelope materialization is this scenario's subject. The
      // test explicitly chooses the unsafe local tier so host bwrap support is
      // not an unrelated precondition; production remains Namespace by default.
      'sandbox_tier = "local"',
    ].join('\n'));
    worker = spawn(workerBinary(), ['--config', workerConfig, '--server', WORKER_BASE], {
      cwd: ROOT,
      env: { ...process.env, AWAKEN_E2E_SHUTDOWN_ON_STDIN_EOF: '1' },
      stdio: ['pipe', 'pipe', 'pipe'],
    });
    worker.stdout.on('data', (chunk) => (output += chunk.toString()));
    worker.stderr.on('data', (chunk) => (output += chunk.toString()));
    const messages = await waitForReply().catch((error) => {
      throw new Error(`${error instanceof Error ? error.message : error}\nworker output:\n${output}`);
    });
    assert.equal(messages.filter((message: any) => String(message.text ?? '').includes('FAKE:seed activation')).length, 1);
    assert.ok(!output.includes(PROVIDER_KEY), 'plaintext provider credential never entered worker logs');
    assert.equal(
      upstream.requests.length,
      1,
      `only the valid pin reached the provider endpoint: ${JSON.stringify(upstream.requests)}`,
    );
    for (const forbidden of [
      'credential.db', 'files.db', 'memory_fs.db', 'resources.db', 'control-seal.key',
    ]) {
      assert.equal(
        fs.existsSync(path.join(storage, 'worker', forbidden)),
        false,
        `${forbidden} must not become Worker authority`,
      );
    }
    console.log('CREDENTIAL MATERIALIZATION WORKER TS E2E PASS: a database-less production Worker consumed one exact recipient-bound projection, called the pinned endpoint, and committed once.');
  } finally {
    if (worker) await stopServer(worker).catch(() => {});
    if (tlsProxy) await stopTlsProxy(tlsProxy).catch(() => {});
    await stopServer(cell).catch(() => {});
    await stopServer(management).catch(() => {});
    upstream.close();
    fs.rmSync(storage, { recursive: true, force: true });
  }
}

main().catch((error) => {
  console.error('CREDENTIAL MATERIALIZATION WORKER TS E2E FAIL:', error);
  process.exitCode = 1;
});
