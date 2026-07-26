// ADR-0067 sealed credential E2E. Cause graph:
// C1 envelope adapter installed -> C2 recipient/live -> C3 target/use binding
// -> C4 payload fingerprint -> provider call. Every failed cause is fail-closed.
//
// | Rule | Recipient/live | Target | Payload | Provider calls |
// |---|---|---|---|---|
// | S1 | T | T | F | 0 |
// | S2 | T | F | T | 0 |
// | S3 | F(expired) | - | - | 0 |
// | S4 | F(recipient) | - | - | 0 |
// | S5 | T | T | T | 1 + committed reply |

import assert from 'node:assert/strict';
import { execFileSync, spawn, type ChildProcessWithoutNullStreams } from 'node:child_process';
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
// @ts-ignore -- the shared JavaScript harness intentionally serves TS scenarios.
import { spawnServer, stopServer, waitForPort } from './harness.mjs';
// @ts-ignore -- the shared JavaScript fixture intentionally serves TS scenarios.
import { startFakeAnthropic } from './fixtures/fake_anthropic_fixture.mjs';

const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const PORT = Number(process.env.E2E_PORT ?? 38923);
const ADMIN_PORT = Number(process.env.E2E_WORKER_PORT ?? 39923);
const BASE = `http://127.0.0.1:${PORT}`;
const THREAD = 'sealed-credential-worker';
const CREDENTIAL_ID = 'sealed-worker-credential';
const CREDENTIAL_REVISION = 7;
const PAYLOAD_FINGERPRINT = 'sha256:sealed-worker-payload';
const PROVIDER_SECRET = 'sk-sealed-worker-e2e'; // awaken-allow: secret

function workerBinary(): string {
  const output = execFileSync(
    'cargo',
    [
      'build', '--quiet', '--message-format=json', '-p', 'awaken-worker',
      '--example', 'sealed_credential_worker',
    ],
    { cwd: ROOT, encoding: 'utf8', maxBuffer: 64 * 1024 * 1024 },
  );
  for (const line of output.split('\n')) {
    if (!line.trim()) continue;
    try {
      const artifact = JSON.parse(line);
      if (artifact.executable && artifact.target?.name === 'sealed_credential_worker') {
        return artifact.executable;
      }
    } catch { /* Cargo emits non-artifact lines too. */ }
  }
  throw new Error('could not resolve sealed credential Worker fixture');
}

async function request(method: string, pathname: string, body?: unknown, worker?: string) {
  const headers: Record<string, string> = {};
  if (body !== undefined) headers['content-type'] = 'application/json';
  if (worker) headers['x-awaken-worker-id'] = worker;
  const response = await fetch(`${BASE}${pathname}`, {
    method,
    headers,
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  const text = await response.text();
  let json: any = null;
  try { json = text ? JSON.parse(text) : null; } catch { /* diagnostics retain text */ }
  return { status: response.status, json, text };
}

async function seedIdentity() {
  const id = 'sealed-seed';
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
          enforced_network_allowlist: false, secret_egress_substitution: false,
          resource_limits: false, custom_rootfs: false,
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
  while (Date.now() <= deadline) {
    const response = await request('GET', `/v1/durable/threads/${THREAD}/messages`);
    const messages = response.json?.messages ?? [];
    if (messages.some((message: any) => String(message.text ?? '').includes('FAKE:seed'))) {
      return messages;
    }
    await new Promise((resolve) => setTimeout(resolve, 50));
  }
  throw new Error('sealed provider reply was not committed');
}

async function main() {
  const storage = mkdtempSync(path.join(tmpdir(), 'awaken-sealed-worker-'));
  const upstream = await startFakeAnthropic(PROVIDER_SECRET);
  const cell = spawnServer('echo', PORT, {
    SESSION_DEPLOYMENT_INGRESS: 'durable',
    SESSION_DEPLOYMENT_STORAGE_DIR: storage,
    SESSION_DEPLOYMENT_DISABLE_LOCAL_POOL: '1',
  }).server;
  let worker: ChildProcessWithoutNullStreams | undefined;
  let workerOutput = '';
  try {
    await waitForPort(PORT);
    const seed = await seedIdentity();
    const submitted = await request(
      'POST', `/v1/durable/threads/${THREAD}-seed/submit_background`, { text: 'seed' },
    );
    assert.equal(submitted.status, 200, submitted.text);
    const seedClaim = await request(
      'POST', '/v1/worker/dispatch/claim', { identity: seed.identity }, seed.id,
    );
    assert.equal(seedClaim.status, 200, seedClaim.text);
    const claimed = seedClaim.json.claimed;
    assert.ok(claimed);

    const providerRef = 'anthropic@1';
    const endpoint = {
      adapter_kind: 'anthropic',
      base_url: `${upstream.url}/v1/`,
      upstream_model: 'sealed-model',
    };
    const access = (recipient: string, expiry: number, fingerprint: string) => ({
      credential: { id: CREDENTIAL_ID, revision: CREDENTIAL_REVISION },
      material_source: 'control_plane_reference',
      envelope: {
        type: 'sealed_for_worker',
        envelope_ref: { id: 'sealed-envelope-1', payload_fingerprint: fingerprint },
        recipient,
        expires_at_unix_ms: expiry,
      },
      usage: { type: 'provider_adapter' },
      policy: {
        allowed_plaintext_holders: [{ boundary: 'worker', trust_domain: 'awaken.worker' }],
        model_exposure: 'forbidden',
      },
    });
    const candidate = (rule: string, candidateEndpoint: any, credential: any) => ({
      ...structuredClone(claimed.request.activation.snapshot.resolved_spec.model_binding),
      model_ref: `sealed-${rule}`,
      backend_ref: 'genai',
      provisioning: {
        type: 'provider', provider_ref: providerRef, route_ref: 'sealed-route@1',
        scope_id: 'sealed-workspace', credential, endpoint: candidateEndpoint,
      },
    });
    const now = Date.now();
    const cases = [
      candidate('bad-payload', endpoint, access('awaken.worker', now + 60_000, 'sha256:wrong')),
      candidate(
        'bad-target',
        { ...endpoint, base_url: 'https://other.invalid/v1/' },
        access('awaken.worker', now + 60_000, PAYLOAD_FINGERPRINT),
      ),
      candidate('expired', endpoint, access('awaken.worker', 0, PAYLOAD_FINGERPRINT)),
      candidate('recipient', endpoint, access('another-worker', now + 60_000, PAYLOAD_FINGERPRINT)),
      candidate('valid', endpoint, access('awaken.worker', now + 60_000, PAYLOAD_FINGERPRINT)),
    ];
    for (const [index, model] of cases.entries()) {
      const dispatch = structuredClone(claimed.request);
      dispatch.activation.run_id = `${claimed.request.activation.run_id}-sealed-${index}`;
      dispatch.activation.thread_id = index === cases.length - 1 ? THREAD : `${THREAD}-${index}`;
      dispatch.session_thread_id = dispatch.activation.thread_id;
      dispatch.activation.snapshot.resolved_spec.model_binding = model;
      dispatch.activation.snapshot.resolved_spec.model_candidates = [];
      dispatch.inference_plaintext_holder = {
        boundary: 'worker', trust_domain: 'awaken.worker',
      };
      dispatch.placement.required_capabilities = ['credential-source/v1', 'native-runtime'];
      const enqueued = await request(
        'POST', '/v1/worker/dispatch/enqueue', { request: dispatch }, seed.id,
      );
      assert.equal(enqueued.status, 200, enqueued.text);
    }
    const settled = await request('POST', '/v1/worker/dispatch/settle', {
      run_id: claimed.lease.run_id,
      epoch: claimed.lease.epoch,
      outcome: 'Done',
      consumed: [],
      identity: seed.identity,
    }, seed.id);
    assert.equal(settled.json.settled, true, settled.text);

    worker = spawn(workerBinary(), [], {
      cwd: ROOT,
      env: {
        ...process.env,
        AWAKEN_UPSTREAM_URL: BASE,
        AWAKEN_WORKER_ID: 'sealed-worker',
        AWAKEN_WORKER_ADMIN_LISTEN: `127.0.0.1:${ADMIN_PORT}`,
        AWAKEN_TEST_PROVIDER_REF: providerRef,
        AWAKEN_TEST_PROVIDER_URL: endpoint.base_url,
        AWAKEN_TEST_PROVIDER_MODEL: endpoint.upstream_model,
        AWAKEN_TEST_WORKSPACE: 'sealed-workspace',
        AWAKEN_TEST_CREDENTIAL_ID: CREDENTIAL_ID,
        AWAKEN_TEST_CREDENTIAL_REVISION: String(CREDENTIAL_REVISION),
        AWAKEN_TEST_PAYLOAD_FINGERPRINT: PAYLOAD_FINGERPRINT,
        AWAKEN_TEST_PROVIDER_SECRET: PROVIDER_SECRET,
      },
      stdio: ['pipe', 'pipe', 'pipe'],
    });
    worker.stdout.on('data', (chunk) => (workerOutput += chunk.toString()));
    worker.stderr.on('data', (chunk) => (workerOutput += chunk.toString()));
    await waitForPort(ADMIN_PORT);
    const messages = await waitForReply().catch((error) => {
      throw new Error(`${error}\nworker output:\n${workerOutput}`);
    });
    assert.equal(
      messages.filter((message: any) => String(message.text ?? '').includes('FAKE:seed')).length,
      1,
    );
    assert.deepEqual(
      upstream.requests.map((request: any) => request.model),
      ['sealed-model'],
      'only S5 reached the real provider',
    );
    assert.ok(!workerOutput.includes(PROVIDER_SECRET), 'plaintext is absent from Worker logs');
    console.log('SEALED CREDENTIAL WORKER TS E2E PASS: exact envelope resolved once; payload, target, expiry and recipient failures stayed fail-closed.');
  } finally {
    if (worker) await stopServer(worker).catch(() => {});
    await stopServer(cell).catch(() => {});
    upstream.close();
    rmSync(storage, { recursive: true, force: true });
  }
}

main().catch((error) => {
  console.error('SEALED CREDENTIAL WORKER TS E2E FAIL:', error);
  process.exitCode = 1;
});
