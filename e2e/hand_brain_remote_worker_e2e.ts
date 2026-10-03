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
import Anthropic, { toFile } from '@anthropic-ai/sdk';
// @ts-expect-error shared JavaScript harness intentionally has no declarations.
import { childDirectories, onlyChildDirectory, pass, spawnServer, stopServer, waitForPort, waitForSessionEventReceipt, waitForValue } from './harness.mjs';
// @ts-expect-error shared JavaScript fixture intentionally has no declarations.
import { startCalcFixture } from './fixtures/mcp_calc_fixture.mjs';
import { alwaysAllowMcpAgent } from './fixtures/managed_mcp_session.ts';
// @ts-expect-error The shared Cargo artifact resolver is intentionally JavaScript.
import { cargoExecutable } from './cargo_binary.mjs';
// @ts-expect-error shared host-capability probe intentionally has no declarations.
import { requireOrSkipBwrap } from './bwrap_capability.mjs';

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
  return cargoExecutable({
    cwd: ROOT,
    packageName: 'awaken-cli',
    targetName: 'hand_brain_lazy_worker',
    targetKind: 'example',
    features: CONTAINER ? [`container-${TIER}`] : [],
  });
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

async function sendAndObserveDispatch(
  client: Anthropic,
  sessionId: string,
  prompt: string,
  expectedSandboxBound: boolean,
  predicate: (observation: { events: any[]; delta: any[] }) => boolean,
  description: string,
): Promise<{ dispatch: any; events: any[] }> {
  const receipt = await client.beta.sessions.events.send(
    sessionId,
    { events: [{ type: 'user.message', content: [{ type: 'text', text: prompt }] }], betas: BETAS },
    { signal: AbortSignal.timeout(CONTAINER ? 60_000 : 15_000) },
  );
  const receiptId = receipt?.data?.[0]?.id;
  if (typeof receiptId !== 'string' || receiptId.length === 0) {
    throw new TypeError('remote Worker send returned no exact Managed Event receipt id');
  }

  // Dispatch/receipt rule D0: C1=POST returns one exact durable receipt;
  // C2=the lifecycle supervisor later exposes its durable dispatch with the
  // scenario's expected Sandbox-binding state; C3=that receipt becomes processed;
  // C4=the caller's Brain/Hand effect and idle commit after it. Effects: E1=capture
  // C2 independently of POST liveness; E2=return only the exact C1-scoped history
  // that satisfies C3+C4. Constraint K1: POST acknowledgement does not imply that
  // C2 already exists, and the Hand rule cannot capture its earlier unbound row;
  // both observers are read-only and never drive the Worker. Rules: !C1=>fail;
  // C1&&!C2=>poll the dispatch authority; C1+C2&&!(C3+C4)=>poll exact-receipt
  // history; all=>E1+E2.
  const observedDispatch = await waitForValue(
    () => dispatch(sessionId),
    (item: any) => item?.sandbox_bound === expectedSandboxBound,
    `timed out observing durable dispatch for ${sessionId}`,
    { timeoutMs: CONTAINER ? 75_000 : 30_000, pollMs: 10 },
  );
  const observation = await waitForSessionEventReceipt(
    client,
    sessionId,
    receiptId,
    BETAS,
    predicate,
    description,
    { timeoutMs: CONTAINER ? 75_000 : 30_000, pollMs: 10 },
  );
  return { dispatch: observedDispatch, events: observation.events };
}

async function dispatch(sessionId: string): Promise<any | undefined> {
  const response = await json('GET', `/v1/durable/threads/${sessionId}/dispatches`);
  return response.dispatches?.[0];
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

export async function runRemoteWorkerScenario(
  { resourceOnly = false }: { resourceOnly?: boolean } = {},
): Promise<void> {
  assert.ok(['local', 'namespace', 'docker', 'podman'].includes(TIER), `supported remote Worker tier: ${TIER}`);
  if (TIER === 'namespace' && !requireOrSkipBwrap()) return;
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
        RUST_LOG: 'awaken_dispatch=debug,awaken_worker=debug,awaken_session_executor=debug',
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

    if (!resourceOnly) {
      const brain = await client.beta.sessions.create({
        agent: alwaysAllowMcpAgent('assistant', [calc]),
        environment_id: environment.id,
        betas: BETAS,
      });
      try {
        var brainObservation = await sendAndObserveDispatch(
          client,
          brain.id,
          'brain',
          false,
          ({ delta }) => delta.some(
            (event) => event.type === 'agent.mcp_tool_use' && event.name === 'mcp__calc__add',
          ) && delta.some((event) => event.type === 'session.status_idle'),
          `${RULES.brain} exact receipt reaches MCP and idle`,
        );
      } catch (error) {
        throw new Error(`${error}\ndispatch=${JSON.stringify(await dispatch(brain.id))}\nworker=${workerOutput}`);
      }
      const brainDispatch = brainObservation.dispatch;
      const brainEvents = brainObservation.events;
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
        var handObservation = await sendAndObserveDispatch(
          client,
          hand.id,
          'hand',
          true,
          ({ delta }) => delta.some(
            (event) => event.type === 'agent.tool_result'
              && !JSON.stringify(event.content).includes('sandbox executor unavailable'),
          ) && delta.some((event) => event.type === 'session.status_idle'),
          `${RULES.hand} exact receipt reaches Hand result and idle`,
        );
      } catch (error) {
        throw new Error(`${error}\ndispatch=${JSON.stringify(await dispatch(hand.id))}\nworker=${workerOutput}`);
      }
      const handDispatch = handObservation.dispatch;
      const handEvents = handObservation.events;
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
    }
    if (resourceOnly && CONTAINER) {
      // Resource decision table: R1 Control freezes one File, live MemoryStore,
      // and binary Skill on a new Session; R2 the remote Worker claims that
      // exact Work and mounts all three into its own container; R3 the Hand
      // reads the File. R1+R2+R3 => one new Worker container, the exact File
      // tool result, current Memory bytes, and exact Skill asset. The Coordinator
      // creates no Sandbox and the Worker retains no node-local Resource authority.
      const fileBytes = Buffer.from('REMOTE-WORKER-FILE-OK');
      const resourceFile = await client.beta.files.upload({
        file: await toFile(fileBytes, 'remote-worker.txt'),
        betas: [...BETAS, 'files-api-2025-04-14'],
      });
      const memoryName = `remote-worker-memory-${process.pid}`;
      const memoryHeaders = { 'anthropic-beta': 'agent-memory-2026-07-22' };
      const memory = await client.post('/v1/memory_stores', {
        body: { name: memoryName }, headers: memoryHeaders,
      }) as { id: string };
      await client.post(`/v1/memory_stores/${memory.id}/memories`, {
        body: { path: '/fact.md', content: 'REMOTE-WORKER-MEMORY-OK' },
        headers: memoryHeaders,
      });
      const skillBytes = Buffer.from([0, 159, 146, 150, 255, 13, 0, 10]);
      const skill = await client.beta.skills.create({
        files: [
          await toFile(Buffer.from('---\nname: remote-resource\ndescription: remote Resource proof\nenvironment: filesystem\n---\nRead the asset.'), 'SKILL.md'),
          await toFile(skillBytes, 'assets/data.bin'),
        ],
        betas: ['skills-2025-10-02'],
      });
      const beforeResources = managedContainerIds();
      const resourceSession = await client.beta.sessions.create({
        agent: {
          id: 'assistant',
          type: 'agent_with_overrides',
          skills: [{ type: 'custom', skill_id: skill.id, version: '1' }],
        },
        environment_id: environment.id,
        resources: [
          { type: 'file', file_id: resourceFile.id, mount_path: '/workspace/missing-remote-hand.txt' },
          { type: 'memory_store', memory_store_id: memory.id },
        ],
        betas: BETAS,
      });
      const resourceObservation = await sendAndObserveDispatch(
        client, resourceSession.id, 'resource-file', true,
        ({ delta }) => delta.some((event) => event.type === 'agent.tool_result'
          && JSON.stringify(event.content).includes(fileBytes.toString()))
          && delta.some((event) => event.type === 'session.status_idle'),
        'remote Worker reads the Control-frozen File and reaches idle',
      );
      assert.ok(resourceObservation.events.some((event) =>
        event.type === 'agent.tool_result' && JSON.stringify(event.content).includes(fileBytes.toString())));
      const resourceContainers = [...managedContainerIds()].filter((id) => !beforeResources.has(id));
      assert.equal(resourceContainers.length, 1, 'one resource Session container belongs to the Worker');
      assert.equal(execFileSync(TIER, ['exec', resourceContainers[0], 'cat',
        `/mnt/memory/${memoryName}/fact.md`], { encoding: 'utf8' }), 'REMOTE-WORKER-MEMORY-OK');
      assert.deepEqual(execFileSync(TIER, ['exec', resourceContainers[0], 'cat',
        `/workspace/.skills/${skill.id}/assets/data.bin`]), skillBytes,
      'remote Worker materializes the exact binary Skill asset');
      for (const localAuthority of ['files.db', 'memory_fs.db', 'resources.db', 'skills']) {
        assert.equal(fs.existsSync(path.join(workerStorage, localAuthority)), false,
          `Worker must read ${localAuthority} from Control, not node-local Resource truth`);
      }
      assert.deepEqual(childDirectories(path.join(controlStorage, 'sandboxes')), [],
        'Coordinator remains free of resource Sandbox state');
      pass(`${TIER} remote Worker materializes Control-frozen File, Skill, and current Memory content`);
    }
    console.log(resourceOnly
      ? `E2E PASS: remote Worker Control-frozen Resource manifest (${TIER}).`
      : `E2E PASS: remote Worker Hand/Brain lazy Sandbox matrix (${TIER}).`);
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

if (path.resolve(process.argv[1] ?? '') === fileURLToPath(import.meta.url)) {
  runRemoteWorkerScenario().catch((error) => {
    console.error('E2E FAIL:', error);
    process.exitCode = 1;
  });
}
