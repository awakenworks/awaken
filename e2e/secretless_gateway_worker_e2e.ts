// Secretless model execution over the real cell + awaken-worker processes.
//
// TypeScript injects an opaque grant into a durable dispatch. A real database-less
// worker claims it over HTTP, passes it to its injected ExecutorProvider, executes
// the returned model, commits through the claimed epoch, and settles the queue.

import assert from 'node:assert/strict';
import { execFileSync, spawn, type ChildProcessWithoutNullStreams } from 'node:child_process';
import fs, { mkdtempSync } from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { spawnServer, stopServer, waitForPort } from './harness.mjs';

const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const PORT = Number(process.env.E2E_PORT ?? 38813);
const WORKER_ADMIN_PORT = Number(process.env.E2E_WORKER_PORT ?? 39813);
const BASE = `http://127.0.0.1:${PORT}`;
const GRANT = 'grant-ts-provider-23';
const THREAD = 'secretless-gateway-worker';

function buildGatewayWorker(): string {
  const output = execFileSync(
    'cargo',
    [
      'build',
      '--quiet',
      '--message-format=json',
      '-p',
      'awaken-worker',
      '--example',
      'gateway_worker',
    ],
    { cwd: ROOT, encoding: 'utf8', maxBuffer: 64 * 1024 * 1024 },
  );
  for (const line of output.split('\n')) {
    if (!line.trim()) continue;
    try {
      const message = JSON.parse(line);
      if (message.executable && message.target?.name === 'gateway_worker') return message.executable;
    } catch {
      // Only Cargo artifact records are relevant.
    }
  }
  throw new Error('could not resolve gateway_worker example');
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
        manifest: {
          manifest_version: 1,
          build_digest: 'secretless-gateway-e2e',
          capabilities: ['host-executor/v1', 'native-runtime'],
          zone: null,
          architecture: process.arch,
          sandbox: {
            isolation: 'workdir',
            tool_transparent: false,
            path_fidelity: false,
            enforced_readonly: false,
            network_isolation: false,
            secret_egress_substitution: false,
            resource_limits: false,
            custom_rootfs: false,
          },
          sandbox_backends: [],
          dispatch_contract: { min: 1, max: 1 },
          runtime_protocol: { min: 1, max: 1 },
          checkpoint_formats: ['stream-v1'],
          capacity: { max_concurrent: 1, resources: {} },
        },
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
      if (observed.some((message) => String(message.text ?? '').includes(`gateway-grant:${GRANT}`))) {
        return observed;
      }
    }
    await new Promise((resolve) => setTimeout(resolve, 50));
  }
  throw new Error(`gateway worker never committed its grant-routed reply: ${JSON.stringify(observed)}`);
}

async function main(): Promise<void> {
  const storage = mkdtempSync(path.join(tmpdir(), 'awaken-secretless-worker-'));
  const cell = spawnServer('echo', PORT, {
    AWAKEN_INGRESS: 'durable',
    AWAKEN_STORAGE_DIR: storage,
    AWAKEN_DISABLE_LOCAL_POOL: '1',
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
    const request = structuredClone(seed.request);
    request.activation.run_id = `${seed.request.activation.run_id}-gateway`;
    request.activation.thread_id = THREAD;
    request.session_thread_id = THREAD;
    request.model_access = { scheme: 'cloud-gateway', reference: GRANT };
    request.placement.required_capabilities = ['cloud-gateway', 'native-runtime'];
    await post('/v1/worker/dispatch/enqueue', { request }, 'seed-worker');
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
    delete env.ANTHROPIC_API_KEY;
    delete env.OPENAI_API_KEY;
    Object.assign(env, {
      AWAKEN_UPSTREAM_URL: BASE,
      AWAKEN_INGRESS: 'durable',
      AWAKEN_WORKER_GATEWAY_ONLY: '1',
      AWAKEN_WORKER_CAPABILITIES: 'cloud-gateway',
      AWAKEN_WORKER_ID: 'gateway-worker-ts',
      AWAKEN_WORKER_ADMIN_LISTEN: `127.0.0.1:${WORKER_ADMIN_PORT}`,
    });
    worker = spawn(buildGatewayWorker(), [], { cwd: ROOT, env, stdio: ['pipe', 'pipe', 'pipe'] });
    worker.stdout.on('data', (chunk) => (workerOutput += chunk.toString()));
    worker.stderr.on('data', (chunk) => (workerOutput += chunk.toString()));
    await waitForPort(WORKER_ADMIN_PORT);

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
      messages.filter((message) => String(message.text ?? '').includes(`gateway-grant:${GRANT}`)).length,
      1,
      'the provider-routed model result committed exactly once',
    );
    assert.ok(!workerOutput.includes('provider-key'), 'worker output contains no provider credential');

    const dispatches = await fetch(`${BASE}/v1/durable/threads/${THREAD}/dispatches`);
    assert.equal(dispatches.status, 200);
    assert.equal(((await dispatches.json()) as any).dispatches.length, 0, 'gateway dispatch settled');

    console.log(
      'SECRETLESS GATEWAY WORKER TS E2E PASS: opaque grant reached ExecutorProvider, selected the model executor, committed once and exposed no provider key.',
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
