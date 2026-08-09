// Cause graph:
// C1 coordinator-only durable ingress + C2 compatible registered Worker -> E1 remote claim
// C3 frozen on_tool_use runtime projection + C4 dynamic MCP -> E2 no Sandbox/binding
// C3 + C5 built-in Hand + C6 current claim -> E3 bind, then execute on Worker Sandbox
//
// Decision table:
// W1/W3/W5 (Workdir/Namespace/Container): C1,C2,C3,C4 => MCP event,
// sandbox_bound=false, no Worker Sandbox.
// W2/W4/W6 (Workdir/Namespace/Container): C1,C2,C3,C5,C6 => Hand result,
// sandbox_bound=true, Worker-only directory/container.
// The real resource plane is part of C2: advertising session-resources/v1 without
// installing it would be a false-positive Worker manifest.

import assert from 'node:assert/strict';
import { execFileSync, spawn, spawnSync, type ChildProcessWithoutNullStreams } from 'node:child_process';
import fs, { mkdtempSync } from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import Anthropic from '@anthropic-ai/sdk';
// @ts-expect-error shared JavaScript harness intentionally has no declarations.
import { childDirectories, onlyChildDirectory, pass, spawnServer, stopServer, waitForPort } from './harness.mjs';
// @ts-expect-error shared JavaScript fixture intentionally has no declarations.
import { startCalcFixture } from './fixtures/mcp_calc_fixture.mjs';
import { alwaysAllowMcpAgent } from './fixtures/managed_mcp_session.ts';

const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const PORT = Number(process.env.E2E_PORT ?? 39852);
const BASE = `http://127.0.0.1:${PORT}`;
const BETAS = ['managed-agents-2026-04-01'];
const TIER = process.env.SESSION_ENVIRONMENT_TIER ?? 'local';
const CONTAINER = TIER === 'docker' || TIER === 'podman';
const RULES = CONTAINER
  ? { brain: 'W5', hand: 'W6' }
  : TIER === 'namespace' ? { brain: 'W3', hand: 'W4' } : { brain: 'W1', hand: 'W2' };
const IMAGE = process.env.AWAKEN_TEST_SESSION_IMAGE ?? 'awaken-sandbox:session-e2e';

function buildWorker(): string {
  const features = CONTAINER ? ['--features', `container-${TIER}`] : [];
  const output = execFileSync(
    'cargo',
    ['build', '--quiet', '--message-format=json', '-p', 'awaken-cli', '--example', 'hand_brain_lazy_worker', ...features],
    { cwd: ROOT, encoding: 'utf8', maxBuffer: 64 * 1024 * 1024 },
  );
  for (const line of output.split('\n')) {
    if (!line.trim()) continue;
    try {
      const message = JSON.parse(line);
      if (message.executable && message.target?.name === 'hand_brain_lazy_worker') {
        return message.executable;
      }
    } catch {
      // Cargo diagnostics are not artifact records.
    }
  }
  throw new Error('could not resolve hand_brain_lazy_worker example');
}

function containerAvailable(): boolean {
  return !CONTAINER || spawnSync(TIER, ['info'], { stdio: 'ignore' }).status === 0;
}

function ensureContainerImage(): void {
  if (!CONTAINER) return;
  if (spawnSync(TIER, ['image', 'inspect', IMAGE], { stdio: 'ignore' }).status === 0) return;
  execFileSync('bash', ['deploy/images/sandbox/build.sh', IMAGE, ''], {
    cwd: ROOT,
    env: { ...process.env, CONTAINER_ENGINE: TIER },
    stdio: 'inherit',
  });
}

function managedContainerIds(): Set<string> {
  if (!CONTAINER) return new Set();
  const result = spawnSync(
    TIER,
    ['ps', '-a', '--filter', 'label=awaken.sandbox=1', '--format', '{{.ID}}'],
    { encoding: 'utf8' },
  );
  assert.equal(result.status, 0, `${TIER} lists awaken-managed containers`);
  return new Set(result.stdout.split('\n').filter(Boolean));
}

async function events(client: Anthropic, sessionId: string): Promise<any[]> {
  const observed: any[] = [];
  for await (const event of client.beta.sessions.events.list(sessionId, { betas: BETAS })) {
    observed.push(event);
  }
  return observed;
}

async function send(client: Anthropic, sessionId: string, prompt: string): Promise<void> {
  await client.beta.sessions.events.send(
    sessionId,
    { events: [{ type: 'user.message', content: [{ type: 'text', text: prompt }] }], betas: BETAS },
    { signal: AbortSignal.timeout(CONTAINER ? 60_000 : 15_000) },
  );
}

async function sendAndObserveDispatch(
  client: Anthropic,
  sessionId: string,
  prompt: string,
): Promise<any> {
  let settled = false;
  let requestError: unknown;
  // Attach the rejection handler before observation begins: a container cold
  // start may outlive one poll interval, but it must not become an unhandled
  // rejection that bypasses the dispatch/Worker diagnostics below.
  const request = send(client, sessionId, prompt)
    .catch((error) => { requestError = error; })
    .finally(() => { settled = true; });
  let last: any;
  const deadline = Date.now() + (CONTAINER ? 75_000 : 30_000);
  while (!settled) {
    assert.ok(Date.now() < deadline, `timed out observing durable dispatch for ${sessionId}`);
    const response = await json('GET', `/v1/durable/threads/${sessionId}/dispatches`);
    if (response.dispatches?.[0]) last = response.dispatches[0];
    await new Promise((resolve) => setTimeout(resolve, 10));
  }
  await request;
  if (requestError) throw requestError;
  assert.ok(last, `observed in-flight durable dispatch for ${sessionId}`);
  return last;
}

async function dispatch(sessionId: string): Promise<any> {
  const response = await json('GET', `/v1/durable/threads/${sessionId}/dispatches`);
  const item = response.dispatches?.[0];
  assert.ok(item, `durable dispatch exists for ${sessionId}`);
  return item;
}

async function json(method: string, route: string, body?: unknown): Promise<any> {
  const response = await fetch(`${BASE}${route}`, {
    method,
    headers: {
      'anthropic-beta': BETAS[0],
      ...(body === undefined ? {} : { 'content-type': 'application/json' }),
    },
    body: body === undefined ? undefined : JSON.stringify(body),
    signal: AbortSignal.timeout(15_000),
  });
  const value = await response.text();
  assert.ok(response.ok, `${method} ${route}: ${response.status} ${value}`);
  return value ? JSON.parse(value) : undefined;
}

async function main(): Promise<void> {
  assert.ok(['local', 'namespace', 'docker', 'podman'].includes(TIER), `supported remote Worker tier: ${TIER}`);
  if (!containerAvailable()) {
    if (process.env.AWAKEN_E2E_REQUIRE_CONTAINER === '1') {
      throw new Error(`required ${TIER} runtime is unavailable`);
    }
    console.log(`E2E SKIP: no reachable ${TIER} runtime for remote Worker Hand/Brain matrix.`);
    return;
  }
  ensureContainerImage();
  const root = mkdtempSync(path.join(tmpdir(), `awaken-hand-brain-worker-${TIER}-`));
  const controlStorage = path.join(root, 'control');
  const workerStorage = path.join(root, 'worker');
  fs.mkdirSync(controlStorage, { recursive: true });
  fs.mkdirSync(workerStorage, { recursive: true });
  const fixture = await startCalcFixture(undefined, { allowAnonymous: true });
  const cell = spawnServer('environment-matrix', PORT, {
    SESSION_DEPLOYMENT_INGRESS: 'durable',
    SESSION_DEPLOYMENT_STORAGE_DIR: controlStorage,
    SESSION_DEPLOYMENT_DISABLE_LOCAL_POOL: '1',
  }).server;
  let worker: ChildProcessWithoutNullStreams | undefined;
  let workerOutput = '';
  try {
    await waitForPort(PORT, 180_000, cell);
    worker = spawn(buildWorker(), [], {
      cwd: ROOT,
      env: {
        ...process.env,
        AWAKEN_UPSTREAM_URL: BASE,
        AWAKEN_WORKER_ID: `hand-brain-${TIER}-${process.pid}`,
        AWAKEN_TEST_WORKER_STORAGE_DIR: workerStorage,
        SESSION_ENVIRONMENT_TIER: TIER,
        AWAKEN_TEST_SESSION_IMAGE: IMAGE,
        RUST_LOG: 'awaken_run_ingress=debug,awaken_worker=debug,awaken_runtime_host=debug',
      },
      stdio: ['pipe', 'pipe', 'pipe'],
    });
    worker.stdout.on('data', (chunk) => { workerOutput += chunk.toString(); });
    worker.stderr.on('data', (chunk) => { workerOutput += chunk.toString(); });
    await new Promise((resolve) => setTimeout(resolve, 750));

    const environment = await json('POST', '/v1/environments', {
      name: `remote-lazy-${TIER}`,
      config: { type: 'self_hosted' },
    });
    const policy = await json('POST', '/v1/awaken/sandbox-execution-policies', {
      id: `remote-lazy-${TIER}-${process.pid}`,
      config: {},
      provisioning: 'on_tool_use',
      disabled: false,
    });
    await json('POST', `/v1/awaken/environments/${environment.id}/sandbox-execution-policy`, {
      policy_id: policy.id,
      version: policy.version,
    });
    const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: BASE });
    const calc = { name: 'calc', type: 'url' as const, url: fixture.url };
    const initialManagedContainers = managedContainerIds();

    const brain = await client.beta.sessions.create({
      agent: alwaysAllowMcpAgent('assistant', [calc]),
      environment_id: environment.id,
      betas: BETAS,
    });
    try {
      var brainDispatch = await sendAndObserveDispatch(client, brain.id, 'brain');
    } catch (error) {
      throw new Error(`${error}\ndispatch=${JSON.stringify(await dispatch(brain.id))}\nworker=${workerOutput}`);
    }
    const brainEvents = await events(client, brain.id);
    assert.ok(
      brainEvents.some((event) => event.type === 'agent.mcp_tool_use' && event.name === 'mcp__calc__add'),
      `${RULES.brain} remote Worker executes MCP in Brain: ${JSON.stringify(brainEvents)}\n${workerOutput}`,
    );
    assert.equal(brainDispatch.sandbox_bound, false, `${RULES.brain} durable dispatch has no Sandbox binding`);
    assert.equal(
      CONTAINER
        ? [...managedContainerIds()].some((id) => !initialManagedContainers.has(id))
        : childDirectories(path.join(workerStorage, 'sandboxes')).length > 0,
      false,
      `${RULES.brain} creates no Worker Sandbox`,
    );
    pass(`${RULES.brain} ${TIER} remote Worker executes dynamic MCP without a Sandbox`);

    const hand = await client.beta.sessions.create({
      agent: 'assistant',
      environment_id: environment.id,
      betas: BETAS,
    });
    try {
      var handDispatch = await sendAndObserveDispatch(client, hand.id, 'hand');
    } catch (error) {
      throw new Error(`${error}\ndispatch=${JSON.stringify(await dispatch(hand.id))}\nworker=${workerOutput}`);
    }
    const handEvents = await events(client, hand.id);
    assert.ok(
      handEvents.some((event) => event.type === 'agent.tool_use' && event.name === 'read'),
      `${RULES.hand} remote Hand call is visible: ${JSON.stringify(handEvents)}\n${workerOutput}`,
    );
    assert.ok(
      handEvents.some((event) => event.type === 'agent.tool_result'
        && !JSON.stringify(event.content).includes('sandbox executor unavailable')),
      `${RULES.hand} remote Hand placement succeeds: ${JSON.stringify(handEvents)}\n${workerOutput}`,
    );
    assert.equal(handDispatch.sandbox_bound, true, `${RULES.hand} Control persists the Worker-created Sandbox binding`);
    if (CONTAINER) {
      const created = [...managedContainerIds()].filter((id) => !initialManagedContainers.has(id));
      assert.equal(created.length, 1, `${RULES.hand} one managed Sandbox lives on remote Worker`);
    } else {
      onlyChildDirectory(
        path.join(workerStorage, 'sandboxes'),
        `${RULES.hand} one opaque Sandbox lives on the remote Worker`,
      );
    }
    assert.deepEqual(
      childDirectories(path.join(controlStorage, 'sandboxes')),
      [],
      'coordinator created no Sandbox',
    );
    pass(`${RULES.hand} first Hand tool lazily materializes the ${TIER} Sandbox on the remote Worker`);
    console.log(`E2E PASS: remote Worker Hand/Brain lazy Sandbox matrix (${TIER}).`);
  } finally {
    if (worker) {
      // The Worker is a long-lived poller and does not treat stdin EOF as a
      // shutdown request; signal it before using the shared exit waiter.
      worker.kill('SIGINT');
      await stopServer(worker).catch(() => {});
    }
    await stopServer(cell).catch(() => {});
    await fixture.close();
    fs.rmSync(root, { recursive: true, force: true });
  }
}

main().catch((error) => {
  console.error('E2E FAIL:', error);
  process.exitCode = 1;
});
