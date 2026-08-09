// Cause/effect E2E for a frozen Session resource manifest crossing the durable
// cell -> remote worker boundary. The worker owns no resource truth: it opens the
// shared Postgres resource ports, validates the carried Workspace, then realizes
// and later revokes the exact immutable File projection in its own sandbox.

import assert from 'node:assert/strict';
import { execFileSync, spawn, type ChildProcessWithoutNullStreams } from 'node:child_process';
import fs, { mkdtempSync } from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import {
  childDirectories,
  deploymentEnv,
  onlyChildDirectory,
  spawnServer,
  stopServer,
  waitForPort,
  waitForValue,
} from './harness.mjs';

const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const PORT = Number(process.env.E2E_PORT ?? 38817);
const WORKER_ADMIN_PORT = Number(process.env.E2E_WORKER_PORT ?? 39817);
const BASE = `http://127.0.0.1:${PORT}`;
const CONFIG_BASE = BASE;
const WORKSPACE = `worker-resource-${process.pid}`;
const THREAD = `resource-session-${process.pid}`;
const GRANT = 'resource-manifest-e2e';
const GRANT_REVISION = 1;
const FILE_BYTES = Buffer.from('immutable input selected by the frozen Session manifest\n');
const MOUNT_PATH = 'uploads/input.txt';
const SKILL_NAME = `remote-worker-skill-${process.pid}`;
const SKILL_BINARY = Buffer.from([0, 159, 146, 150, 255, 13, 0, 10]);
const MEMORY_THREAD = `resource-memory-session-${process.pid}`;
const MEMORY_BYTES = Buffer.from('mutable memory content from shared resource truth');

const sleep = (milliseconds: number) => new Promise((resolve) => setTimeout(resolve, milliseconds));

function docker(...args: string[]): string {
  return execFileSync('docker', args, { cwd: ROOT, encoding: 'utf8' }).trim();
}

async function postgres(): Promise<{ container?: string; url: string }> {
  if (process.env.SESSION_DEPLOYMENT_DATABASE_URL) {
    return {
      container: process.env.AWAKEN_E2E_POSTGRES_CONTAINER,
      url: process.env.SESSION_DEPLOYMENT_DATABASE_URL,
    };
  }
  const container = `awaken-worker-resource-pg-${process.pid}`;
  docker(
    'run', '-d', '--name', container,
    '-e', 'POSTGRES_PASSWORD=test',
    '-e', 'POSTGRES_DB=awaken',
    '-p', '127.0.0.1::5432',
    '--health-cmd=pg_isready -U postgres -d awaken',
    '--health-interval=1s', '--health-timeout=2s', '--health-retries=30',
    'postgres:16-alpine',
  );
  const deadline = Date.now() + 60_000;
  while (Date.now() < deadline) {
    const health = docker('inspect', '--format', '{{.State.Health.Status}}', container);
    if (health === 'healthy') {
      const mapping = docker('port', container, '5432/tcp').split('\n')[0];
      const port = mapping.slice(mapping.lastIndexOf(':') + 1);
      return { container, url: `postgres://postgres:test@127.0.0.1:${port}/awaken` };
    }
    if (health === 'unhealthy') throw new Error('disposable Postgres became unhealthy');
    await sleep(250);
  }
  throw new Error('timed out waiting for disposable Postgres');
}

function buildWorker(): string {
  const output = execFileSync(
    'cargo',
    [
      'build',
      '--quiet',
      '--message-format=json',
      '-p',
      'awaken-cli',
      '--example',
      'credential_reference_worker',
    ],
    { cwd: ROOT, encoding: 'utf8', maxBuffer: 64 * 1024 * 1024 },
  );
  for (const line of output.split('\n')) {
    if (!line.trim()) continue;
    try {
      const message = JSON.parse(line);
      if (message.executable && message.target?.name === 'credential_reference_worker') {
        return message.executable;
      }
    } catch {
      // Only Cargo artifact records are relevant.
    }
  }
  throw new Error('could not resolve credential_reference_worker example');
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

async function resourceRequest(method: string, pathname: string, body?: unknown): Promise<any> {
  const response = await fetch(`${CONFIG_BASE}/v1/workspaces/${WORKSPACE}/${pathname}`, {
    method,
    headers: body === undefined ? undefined : { 'content-type': 'application/json' },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  const text = await response.text();
  assert.equal(response.status, 200, `${method} ${pathname} accepted: ${text}`);
  return text ? JSON.parse(text) : {};
}

async function uploadFile(): Promise<string> {
  const form = new FormData();
  form.append('purpose', 'agent');
  form.append('file', new Blob([FILE_BYTES]), 'input.txt');
  const response = await fetch(`${CONFIG_BASE}/v1/workspaces/${WORKSPACE}/files`, {
    method: 'POST',
    body: form,
  });
  const text = await response.text();
  assert.equal(response.status, 200, `file upload accepted: ${text}`);
  return JSON.parse(text).id;
}

async function uploadSkill(): Promise<{ skill_id: string; version: number; bundle_sha256: string }> {
  const form = new FormData();
  form.append(
    'file',
    new Blob([
      `---\nname: ${SKILL_NAME}\ndescription: frozen remote worker Skill\n---\nRead the supporting asset.`,
    ], { type: 'text/markdown' }),
    'SKILL.md',
  );
  form.append(
    'file',
    new Blob([SKILL_BINARY], { type: 'application/octet-stream' }),
    'assets/data.bin',
  );
  const created = await fetch(`${CONFIG_BASE}/v1/workspaces/${WORKSPACE}/skills`, {
    method: 'POST',
    body: form,
  });
  const createdText = await created.text();
  assert.equal(created.status, 200, `Skill upload accepted: ${createdText}`);
  const skillId = JSON.parse(createdText).id;
  const version = await fetch(
    `${CONFIG_BASE}/v1/workspaces/${WORKSPACE}/skills/${skillId}/versions/1`,
  );
  const versionText = await version.text();
  assert.equal(version.status, 200, `Skill version retrieved: ${versionText}`);
  const projected = JSON.parse(versionText);
  return {
    skill_id: skillId,
    version: Number(projected.version),
    bundle_sha256: projected.bundle_sha256,
  };
}

async function createMemory(): Promise<{ memory_store_id: string; config: any }> {
  const created = await resourceRequest('POST', 'memory_stores', {
    name: `remote-worker-memory-${process.pid}`,
    description: 'shared mutable Memory input',
  });
  // The worker envelope carries the Coordinator-resolved, secret-free resource
  // snapshot. This is not a public behavior-authoring API.
  const config = {
    memory_store_id: created.id,
    version: 1,
    retention_policy: {},
  };
  await resourceRequest('POST', `memory_stores/${created.id}/memories`, {
    path: '/fact.md',
    content: MEMORY_BYTES.toString(),
  });
  return { memory_store_id: created.id, config };
}

async function registerSeedWorker(): Promise<{ id: string; identity: any }> {
  const id = `resource-seed-${process.pid}`;
  const registration = await post(
    '/v1/worker/register',
    {
      registration: {
        worker_id: id,
        incarnation_id: `${id}-incarnation`,
        manifest: {
          manifest_version: 1,
          build_digest: 'resource-manifest-seed',
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
    id,
  );
  const identity = registration.worker?.snapshot?.identity;
  assert.ok(identity, 'seed registration returned a durable identity');
  const heartbeat = await post(
    '/v1/worker/heartbeat',
    { identity, heartbeat: { sequence: 1, ready: true, in_flight: 0 } },
    id,
  );
  assert.equal(heartbeat.mutation, 'applied');
  return { id, identity };
}

function resourceEnvelope(
  fileId?: string,
  workspace = WORKSPACE,
  skill?: any,
  memory?: { memory_store_id: string; config: any },
  resourceRevision = 1,
): any {
  const inputs = fileId === undefined
    ? []
    : [{
        binding_id: 'session-file',
        source: { kind: 'file', file_id: fileId },
        mount_path: MOUNT_PATH,
        access: 'read_only',
      }];
  if (memory !== undefined) {
    inputs.push({
      binding_id: 'session-memory',
      source: {
        kind: 'memory_store',
        memory_store_id: memory.memory_store_id,
        config: memory.config,
      },
      mount_path: 'memory',
      access: 'read_write',
    });
  }
  return {
    workspace_id: workspace,
    // Resource-generation decision table: exact generation + exact manifest is
    // an idempotent replay; higher generation may replace it; equal/lower
    // generation with different content is fenced before mutation.
    resource_revision: resourceRevision,
    resolved_resources_json: JSON.stringify({ inputs, skills: skill === undefined ? [] : [skill] }),
  };
}

function runRequest(
  seed: any,
  suffix: string,
  envelope: any,
  thread = THREAD,
  skillIds: string[] = [],
): any {
  const request = structuredClone(seed);
  request.activation.run_id = `${seed.activation.run_id}-${suffix}`;
  request.activation.thread_id = thread;
  request.session_thread_id = thread;
  request.activation.snapshot.resolved_spec.model_binding = {
    ...structuredClone(request.activation.snapshot.resolved_spec.model_binding),
    provisioning: {
      type: 'provider',
      provider_ref: 'fixture-provider@1',
      route_ref: 'fixture-worker-local@1',
      scope_id: WORKSPACE,
      credential: {
        credential: { id: GRANT, revision: GRANT_REVISION },
        material_source: 'worker_reference',
        usage: { type: 'provider_adapter' },
        policy: {
          allowed_plaintext_holders: [
            { boundary: 'worker', trust_domain: 'awaken.worker' },
          ],
          model_exposure: 'forbidden',
        },
      },
      endpoint: {
        adapter_kind: 'fixture',
        base_url: 'https://worker-local.invalid',
        upstream_model: request.activation.snapshot.resolved_spec.model_binding.model_ref,
      },
    },
  };
  request.activation.snapshot.resolved_spec.model_candidates = [];
  // Skill intersection decision table: the canonical Agent `skills` field
  // grants selected identities; `session_resources.skills` freezes each exact
  // version/hash. Present in both => deliver the frozen bytes; absent from Agent
  // selection => do not expose it; absent from the manifest => no bytes to
  // materialize. Never add the legacy `skill_ids` alias beside `skills`.
  request.activation.snapshot.resolved_spec.plugin_config.agent.skills = skillIds;
  request.inference_plaintext_holder = {
    boundary: 'worker', trust_domain: 'awaken.worker',
  };
  request.execution_scope = WORKSPACE;
  request.session_resources = envelope;
  request.placement.required_capabilities = [
    'worker-local-credentials/v1',
    'native-runtime',
    'session-resources/v1',
  ];
  request.placement.required_credentials = [{ id: GRANT, revision: GRANT_REVISION }];
  return request;
}

async function waitUntilSettled(thread: string, timeoutMs = 30_000): Promise<void> {
  const deadline = Date.now() + timeoutMs;
  let last = '';
  while (Date.now() <= deadline) {
    const response = await fetch(`${BASE}/v1/durable/threads/${thread}/dispatches`);
    last = await response.text();
    if (response.status === 200 && ((JSON.parse(last) as any).dispatches ?? []).length === 0) return;
    await sleep(50);
  }
  throw new Error(`thread ${thread} did not settle: ${last}`);
}

async function waitForDispatchStatus(
  thread: string,
  expected: string,
  timeoutMs = 30_000,
): Promise<any> {
  const deadline = Date.now() + timeoutMs;
  let last = '';
  while (Date.now() <= deadline) {
    const response = await fetch(`${BASE}/v1/durable/threads/${thread}/dispatches`);
    last = await response.text();
    if (response.status === 200) {
      const dispatch = ((JSON.parse(last) as any).dispatches ?? [])[0];
      if (dispatch?.status === expected) return dispatch;
    }
    await sleep(50);
  }
  throw new Error(`thread ${thread} never reached ${expected}: ${last}`);
}

async function waitForFile(file: string, expected: Buffer | undefined, timeoutMs = 30_000): Promise<void> {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() <= deadline) {
    if (expected === undefined) {
      if (!fs.existsSync(file)) return;
    } else if (fs.existsSync(file) && fs.readFileSync(file).equals(expected)) {
      return;
    }
    await sleep(50);
  }
  const observed = fs.existsSync(file) ? fs.readFileSync(file).toString('hex') : '<missing>';
  throw new Error(`sandbox projection ${file} did not converge; observed=${observed}`);
}

async function enqueueAndAwait(request: any, seedWorkerId: string): Promise<void> {
  await post('/v1/worker/dispatch/enqueue', { request }, seedWorkerId);
  await waitUntilSettled(request.session_thread_id);
}

async function main(): Promise<void> {
  const database = await postgres();
  const configStorage = mkdtempSync(path.join(tmpdir(), 'awaken-resource-config-'));
  const workerStorage = mkdtempSync(path.join(tmpdir(), 'awaken-resource-worker-'));
  // Seed-model decision table: R1 explicit scenario model -> the production
  // management composition emits one real serialized activation for this
  // resource-boundary test; R2 production with no configured model -> durable
  // submission fails closed (covered by the no-model Host tests), never an
  // implicit echo fallback. Resource/Postgres/dispatch/Worker adapters remain
  // the production implementations in both rules.
  const management = spawnServer(
    'management',
    PORT,
    {
      ...deploymentEnv(configStorage, {
        // Management-plane identity decision table for this resource-boundary
        // scenario: no-login + no Authorization => exercise the resource
        // contract; embedded/cloud IAM + no Authorization => reject before
        // multipart ingestion. Authentication behavior has its own E2Es, so
        // this test selects no-login explicitly instead of weakening the
        // production-ready Embedded IAM default or retrying an authorization
        // failure as a transport error.
        identityMode: 'no-login',
        controlSealKey:
          '00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff',
        databases: {
          resource_database_url: database.url,
          admin_db: database.url,
          sessions_db: database.url,
          runtime_database_url: database.url,
        },
        // Coordinator/Resources authority decision table: shared runtime plus
        // `run_local_pool=false` => this production process owns dispatch and
        // exact Resource reads while only the registered remote Worker may
        // execute; a separate scenario cell would own a second Resource store
        // and could not prove the cross-process projection contract.
        fields: { run_local_pool: false, acp_clis: [] },
      }),
      AWAKEN_SCENARIO_WORKSPACE: WORKSPACE,
    },
  ).server;
  let worker: ChildProcessWithoutNullStreams | undefined;
  let workerOutput = '';
  try {
    await waitForPort(PORT, 180_000, management);
    const fileId = await uploadFile();
    const skill = await uploadSkill();
    const memory = await createMemory();
    const seedWorker = await registerSeedWorker();

    await post(`/v1/durable/threads/${THREAD}-seed/submit_background`, { text: 'seed activation' });
    const seedClaim = (
      await post('/v1/worker/dispatch/claim', { identity: seedWorker.identity }, seedWorker.id)
    ).claimed;
    assert.ok(seedClaim, 'seed worker claimed a server-created activation');
    const first = runRequest(
      seedClaim.request,
      'attach',
      resourceEnvelope(fileId, WORKSPACE, skill),
      THREAD,
      [skill.skill_id],
    );
    await post('/v1/worker/dispatch/enqueue', { request: first }, seedWorker.id);

    // The manifest itself causes a placement requirement. A worker without the
    // resource preparer must not claim it, including before any sandbox exists.
    const ineligible = await post(
      '/v1/worker/dispatch/claim',
      { identity: seedWorker.identity },
      seedWorker.id,
    );
    assert.equal(ineligible.claimed, null, 'resource-ineligible worker cannot claim the manifest');
    const seedSettle = await post(
      '/v1/worker/dispatch/settle',
      {
        run_id: seedClaim.lease.run_id,
        epoch: seedClaim.lease.epoch,
        outcome: 'Done',
        consumed: [],
        identity: seedWorker.identity,
      },
      seedWorker.id,
    );
    assert.equal(seedSettle.settled, true);

    const env = { ...process.env } as Record<string, string>;
    delete env.ANTHROPIC_API_KEY;
    delete env.OPENAI_API_KEY;
    Object.assign(env, {
      AWAKEN_UPSTREAM_URL: BASE,
      SESSION_DEPLOYMENT_INGRESS: 'durable',
      SESSION_DEPLOYMENT_STORAGE_DIR: workerStorage,
      AWAKEN_WORKER_GATEWAY_ONLY: '1',
      AWAKEN_TEST_CREDENTIAL_ID: GRANT,
      AWAKEN_TEST_CREDENTIAL_REVISION: String(GRANT_REVISION),
      AWAKEN_TEST_RESOURCE_DATABASE_URL: database.url,
      AWAKEN_TEST_ADMIN_DATABASE_URL: database.url,
      AWAKEN_TEST_WORKER_STORAGE_DIR: workerStorage,
      AWAKEN_WORKER_ID: `resource-worker-${process.pid}`,
      AWAKEN_WORKER_ADMIN_LISTEN: `127.0.0.1:${WORKER_ADMIN_PORT}`,
      AWAKEN_E2E_SHUTDOWN_ON_STDIN_EOF: '1',
    });
    worker = spawn(buildWorker(), [], { cwd: ROOT, env, stdio: ['pipe', 'pipe', 'pipe'] });
    worker.stdout.on('data', (chunk) => (workerOutput += chunk.toString()));
    worker.stderr.on('data', (chunk) => (workerOutput += chunk.toString()));

    await waitUntilSettled(THREAD).catch((error) => {
      throw new Error(`${error instanceof Error ? error.message : error}\nworker output:\n${workerOutput}`);
    });
    const sandboxParent = path.join(workerStorage, 'sandboxes');
    const sandboxRoot = onlyChildDirectory(
      sandboxParent,
      'the first frozen manifest owns one opaque sandbox root',
    );
    // File path decision rule: a requested path already rooted below
    // `mnt/session/uploads` is preserved; every other safe logical path is
    // projected below that public root. `uploads/input.txt` therefore proves
    // the latter as `/mnt/session/uploads/uploads/input.txt`.
    const projectedFile = path.join(sandboxRoot, 'mnt', 'session', 'uploads', MOUNT_PATH);
    const projectedSkill = path.join(
      sandboxRoot,
      'workspace',
      '.skills',
      skill.skill_id,
      'assets',
      'data.bin',
    );
    await waitForFile(projectedFile, FILE_BYTES).catch((error) => {
      const tree = execFileSync('find', [sandboxRoot, '-maxdepth', '6', '-type', 'f'], {
        encoding: 'utf8',
      });
      throw new Error(
        `${error instanceof Error ? error.message : error}\nfiles:\n${tree}\nworker output:\n${workerOutput}`,
      );
    });
    await waitForFile(projectedSkill, SKILL_BINARY).catch((error) => {
      const tree = fs.existsSync(sandboxRoot)
        ? execFileSync('find', [sandboxRoot, '-maxdepth', '5', '-type', 'f'], { encoding: 'utf8' })
        : '<missing sandbox>';
      throw new Error(`${error instanceof Error ? error.message : error}\nfiles:\n${tree}\nworker output:\n${workerOutput}`);
    });

    // An explicit empty successor is semantically meaningful: it must route to a
    // resource-capable worker and remove the projection from the live Session.
    await enqueueAndAwait(
      runRequest(
        seedClaim.request,
        'detach',
        resourceEnvelope(undefined, WORKSPACE, undefined, undefined, 2),
      ),
      seedWorker.id,
    ).catch((error) => {
      throw new Error(
        `${error instanceof Error ? error.message : error}\nworker output:\n${workerOutput}`,
      );
    });
    await waitForFile(projectedFile, undefined);
    await waitForFile(path.join(sandboxRoot, 'workspace', '.skills'), undefined);

    // Rebinding uses the same immutable shared File bytes; neither the cell nor
    // worker consults a node-local resource copy or current Agent defaults.
    await enqueueAndAwait(
      runRequest(
        seedClaim.request,
        'reattach',
        resourceEnvelope(fileId, WORKSPACE, skill, undefined, 3),
        THREAD,
        [skill.skill_id],
      ),
      seedWorker.id,
    );
    await waitForFile(projectedFile, FILE_BYTES);
    await waitForFile(projectedSkill, SKILL_BINARY);
    for (const relative of ['files.db', 'memory_fs.db', 'resources.db', 'skills']) {
      assert.equal(
        fs.existsSync(path.join(workerStorage, relative)),
        false,
        `${relative} must not become node-local worker resource truth`,
      );
    }

    // Mutable Memory content is not copied into the dispatch or pinned by entry
    // revision. The worker opens the pinned store configuration, then realizes the
    // current shared content through the injected MemoryRepository/Mounter ports.
    const memoryRequest = runRequest(
      seedClaim.request,
      'memory',
      resourceEnvelope(undefined, WORKSPACE, undefined, memory),
      MEMORY_THREAD,
    );
    const rootsBeforeMemory = new Set(childDirectories(sandboxParent));
    await enqueueAndAwait(memoryRequest, seedWorker.id);
    const rootsAfterMemory = await waitForValue(
      () => childDirectories(sandboxParent),
      (roots) => roots.length === rootsBeforeMemory.size + 1,
      'Memory projection creates one additional opaque sandbox root',
    );
    const memoryRoot = rootsAfterMemory.find((root) => !rootsBeforeMemory.has(root));
    assert.ok(memoryRoot, 'the Memory Session has one newly-created sandbox root');
    const projectedMemory = path.join(memoryRoot, 'workspace', '.mnt', 'memory', 'fact.md');
    await waitForFile(projectedMemory, MEMORY_BYTES);

    // The immutable config pin cannot revive a resource after a live lifecycle
    // transition. The retry is genuinely claimed, then denied before model use.
    await resourceRequest('POST', `memory_stores/${memory.memory_store_id}/archive`);
    const deniedMemory = runRequest(
      seedClaim.request,
      'memory-archived',
      resourceEnvelope(undefined, WORKSPACE, undefined, memory),
      MEMORY_THREAD,
    );
    await post('/v1/worker/dispatch/enqueue', { request: deniedMemory }, seedWorker.id);
    await waitForDispatchStatus(MEMORY_THREAD, 'Leased');
    const denialDeadline = Date.now() + 10_000;
    while (
      Date.now() <= denialDeadline &&
      !(workerOutput.includes(memory.memory_store_id) && workerOutput.includes('not active'))
    ) {
      await sleep(50);
    }
    assert.ok(
      workerOutput.includes(memory.memory_store_id) && workerOutput.includes('not active'),
      `archived Memory pin was not denied by live state:\n${workerOutput}`,
    );

    // Workspace equality is checked before opening a sandbox. Give the bad run a
    // fresh thread so absence of its sandbox is externally observable.
    const foreignThread = `${THREAD}-foreign`;
    const foreign = runRequest(
      seedClaim.request,
      'foreign',
      resourceEnvelope(fileId, `${WORKSPACE}-other`, skill),
    );
    foreign.activation.thread_id = foreignThread;
    foreign.session_thread_id = foreignThread;
    const rootsBeforeForeign = childDirectories(sandboxParent);
    await post('/v1/worker/dispatch/enqueue', { request: foreign }, seedWorker.id);
    await waitForDispatchStatus(foreignThread, 'Leased');
    assert.deepEqual(
      childDirectories(sandboxParent),
      rootsBeforeForeign,
      'scope-mismatched resource dispatch failed before sandbox creation',
    );

    assert.ok(!workerOutput.includes(FILE_BYTES.toString()), 'worker logs do not expose File bytes');
    console.log(
      'WORKER RESOURCE MANIFEST TS E2E PASS: frozen File/Skill/Memory realization, exact-tree detach, live Memory deny, capability placement, and cross-Workspace failure crossed real cell/worker processes.',
    );
  } finally {
    if (worker) await stopServer(worker).catch(() => {});
    await stopServer(management).catch(() => {});
    fs.rmSync(configStorage, { recursive: true, force: true });
    fs.rmSync(workerStorage, { recursive: true, force: true });
    if (database.container && !process.env.AWAKEN_E2E_POSTGRES_CONTAINER) {
      try {
        docker('rm', '-f', database.container);
      } catch (error) {
        console.error(`failed to remove disposable Postgres ${database.container}: ${error}`);
      }
    }
  }
}

main().catch((error) => {
  console.error('WORKER RESOURCE MANIFEST TS E2E FAIL:', error);
  process.exitCode = 1;
});
