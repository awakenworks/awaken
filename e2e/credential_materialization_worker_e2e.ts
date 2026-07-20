// Production worker E2E: awaken-worker opens only the shared credential backend,
// materializes the exact access pinned in a dispatch, calls that endpoint, and
// commits the provider reply through the cell server.

import assert from 'node:assert/strict';
import { execFileSync, spawn, type ChildProcessWithoutNullStreams } from 'node:child_process';
import fs, { mkdtempSync } from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { spawnServer, stopServer, waitForPort } from './harness.mjs';
import { startFakeAnthropic } from './fixtures/fake_anthropic_fixture.mjs';

const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const PORT = Number(process.env.E2E_PORT ?? 38823);
const ADMIN_PORT = Number(process.env.E2E_WORKER_PORT ?? 39823);
const CONFIG_PORT = Number(process.env.E2E_CONFIG_PORT ?? 40823);
const BASE = `http://127.0.0.1:${PORT}`;
const CONFIG_BASE = `http://127.0.0.1:${CONFIG_PORT}`;
const THREAD = 'credential-materialization-worker';
const SEAL_KEY = '1234567890abcdef1234567890abcdef1234567890abcdef1234567890abcdef';
const PROVIDER_KEY = 'sk-worker-materialization-e2e'; // awaken-allow: secret

function workerBinary(): string {
  const output = execFileSync(
    'cargo',
    ['build', '--quiet', '--message-format=json', '-p', 'awaken-worker', '--bin', 'awaken-worker'],
    { cwd: ROOT, encoding: 'utf8', maxBuffer: 64 * 1024 * 1024 },
  );
  for (const line of output.split('\n')) {
    if (!line.trim()) continue;
    try {
      const artifact = JSON.parse(line);
      if (artifact.executable && artifact.target?.name === 'awaken-worker') return artifact.executable;
    } catch { /* only Cargo artifact records matter */ }
  }
  throw new Error('could not resolve awaken-worker binary');
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
  const upstream = await startFakeAnthropic(PROVIDER_KEY);
  const management = spawnServer('management', CONFIG_PORT, {
    AWAKEN_MGMT_DIR: storage,
    AWAKEN_MGMT_SEAL_KEY: SEAL_KEY,
  }).server;
  const cell = spawnServer('echo', PORT, {
    AWAKEN_INGRESS: 'durable',
    AWAKEN_STORAGE_DIR: storage,
    AWAKEN_MGMT_DIR: storage,
    AWAKEN_MGMT_SEAL_KEY: SEAL_KEY,
    AWAKEN_DISABLE_LOCAL_POOL: '1',
  }).server;
  let worker: ChildProcessWithoutNullStreams | undefined;
  let output = '';
  try {
    await waitForPort(CONFIG_PORT);
    await waitForPort(PORT);
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
    dispatch.session_thread_id = THREAD;
    dispatch.activation.snapshot.metadata = {
      source: { agent_id: '', revision: 0 },
      publication_version: '',
      resolution: { inputs: [] },
      fingerprint: '',
      inference_access: {
        scheme: 'credential-source/v1',
        reference: credential.json.id,
        provider_ref: 'anthropic@1',
        route_ref: 'fake-endpoint@1',
        scope_id: credential.json.workspace_id,
        credential_access: {
          credential: { id: credential.json.id, revision: credential.json.version },
          injection: 'reference',
          usage: { type: 'provider_adapter' },
        },
        endpoint: {
          adapter_kind: 'anthropic',
          base_url: `${upstream.url}/v1/`,
          upstream_model: 'fake-worker-model',
        },
      },
    };
    dispatch.placement.required_capabilities = ['credential-source/v1', 'native-runtime'];

    // The same production Worker must fail closed for malformed or stale pins
    // and continue draining. Dispatch retry policy retains failed attempts; none
    // may reach a provider or fall back to catalog resolution.
    const invalidAccesses = [
      { ...structuredClone(dispatch.activation.snapshot.metadata.inference_access), scheme: 'unknown/v1' },
      {
        ...structuredClone(dispatch.activation.snapshot.metadata.inference_access),
        credential_access: {
          ...structuredClone(dispatch.activation.snapshot.metadata.inference_access.credential_access),
          credential: { id: 'different-credential', revision: credential.json.version },
        },
      },
      { ...structuredClone(dispatch.activation.snapshot.metadata.inference_access), scope_id: 'foreign-workspace' },
      {
        ...structuredClone(dispatch.activation.snapshot.metadata.inference_access),
        endpoint: {
          ...structuredClone(dispatch.activation.snapshot.metadata.inference_access.endpoint),
          upstream_model: '',
        },
      },
      {
        ...structuredClone(dispatch.activation.snapshot.metadata.inference_access),
        candidates: [{
          model_ref: 'a-different-model',
          access: structuredClone(dispatch.activation.snapshot.metadata.inference_access),
        }],
      },
    ];
    for (const [index, inferenceAccess] of invalidAccesses.entries()) {
      const invalid = structuredClone(dispatch);
      const thread = `${THREAD}-invalid-${index}`;
      invalid.activation.run_id = `${claimed.request.activation.run_id}-invalid-${index}`;
      invalid.activation.thread_id = thread;
      invalid.session_thread_id = thread;
      invalid.activation.snapshot.metadata.inference_access = inferenceAccess;
      assert.equal((await request('POST', '/v1/worker/dispatch/enqueue', { request: invalid }, seed.id)).status, 200);
    }
    assert.equal((await request('POST', '/v1/worker/dispatch/enqueue', { request: dispatch }, seed.id)).status, 200);
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
        AWAKEN_INGRESS: 'durable',
        AWAKEN_MGMT_DIR: storage,
        AWAKEN_MGMT_SEAL_KEY: SEAL_KEY,
        AWAKEN_WORKER_ID: 'materialization-worker',
        AWAKEN_WORKER_ADMIN_LISTEN: `127.0.0.1:${ADMIN_PORT}`,
      },
      stdio: ['pipe', 'pipe', 'pipe'],
    });
    worker.stdout.on('data', (chunk) => (output += chunk.toString()));
    worker.stderr.on('data', (chunk) => (output += chunk.toString()));
    await waitForPort(ADMIN_PORT);
    const messages = await waitForReply().catch((error) => {
      throw new Error(`${error instanceof Error ? error.message : error}\nworker output:\n${output}`);
    });
    assert.equal(messages.filter((message: any) => String(message.text ?? '').includes('FAKE:seed activation')).length, 1);
    assert.ok(!output.includes(PROVIDER_KEY), 'plaintext provider credential never entered worker logs');
    assert.equal(upstream.requests.length, 1, 'only the valid pin reached the provider endpoint');
    console.log('CREDENTIAL MATERIALIZATION WORKER TS E2E PASS: production worker opened credential stores, injected the exact pinned revision, called the pinned endpoint, and committed once.');
  } finally {
    if (worker) await stopServer(worker).catch(() => {});
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
