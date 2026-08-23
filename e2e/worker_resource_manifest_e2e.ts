// Cause/effect E2E for a frozen Session resource manifest crossing the durable
// cell -> remote worker boundary. The worker owns no resource truth: it opens the
// shared Postgres resource ports, validates the carried Workspace, then realizes
// and later revokes the exact immutable File projection in its own sandbox.

import assert from 'node:assert/strict';
import {
  execFileSync,
  spawn,
  spawnSync,
  type ChildProcessWithoutNullStreams,
} from 'node:child_process';
import { createHash } from 'node:crypto';
import fs, { mkdtempSync } from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import {
  WORKER_PROVIDER_CREDENTIAL_CAPABILITY,
  spawnProduction,
  stopServer,
  waitForPort,
  waitForValue,
} from './harness.mjs';
import { cargoExecutable } from './cargo_binary.mjs';
import { startFakeAnthropic } from './fixtures/fake_anthropic_fixture.mjs';
import { nativeProviderCandidateFixture } from './fixtures/provider_candidate_fixture.mjs';
import {
  claimedCommitRequestFixture,
  terminalThreadCommitFixture,
} from './fixtures/thread_commit_fixture.mjs';

const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const PORT = Number(process.env.E2E_PORT ?? 38817);
const WORKER_ADMIN_PORT = Number(process.env.E2E_WORKER_PORT ?? 39817);
const INTERNAL_PORT = Number(process.env.E2E_CONFIG_PORT ?? 40817);
const BASE = `http://127.0.0.1:${PORT}`;
const INTERNAL_BASE = `http://127.0.0.1:${INTERNAL_PORT}`;
const CONFIG_BASE = BASE;
let WORKSPACE = `worker-resource-${process.pid}`;
const THREAD = `resource-session-${process.pid}`;
const GRANT = 'resource-manifest-e2e';
const GRANT_REVISION = 1;
const SEED_MODEL = `resource-seed-model-${process.pid}`;
const SEED_AGENT = `resource-seed-agent-${process.pid}`;
const SEED_KEY = 'sk-resource-seed';
const MEMORY_BETA = 'agent-memory-2026-07-22';
const SKILLS_BETA = 'skills-2025-10-02';
const FILES_BETA = 'files-api-2025-04-14';
const FILE_BYTES = Buffer.from('immutable input selected by the frozen Session manifest\n');
const MOUNT_PATH = 'uploads/input.txt';
const SKILL_NAME = `remote-worker-skill-${process.pid}`;
const SKILL_BINARY = Buffer.from([0, 159, 146, 150, 255, 13, 0, 10]);
const MEMORY_THREAD = `resource-memory-session-${process.pid}`;
const MEMORY_BYTES = Buffer.from('mutable memory content from shared resource truth');
const SESSION_IMAGE = process.env.AWAKEN_TEST_SESSION_IMAGE ?? 'awaken-sandbox:session-e2e';

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
  return cargoExecutable({
    cwd: ROOT,
    packageName: 'awaken-cli',
    targetName: 'credential_reference_worker',
    targetKind: 'example',
    features: ['container-docker'],
  });
}

async function post(pathname: string, body: unknown, worker?: string): Promise<any> {
  const headers: Record<string, string> = { 'content-type': 'application/json' };
  if (worker) headers['x-awaken-worker-id'] = worker;
  const authority = pathname.startsWith('/v1/worker/') ? INTERNAL_BASE : BASE;
  const response = await fetch(`${authority}${pathname}`, {
    method: 'POST',
    headers,
    body: JSON.stringify(body),
  });
  const text = await response.text();
  assert.equal(response.status, 200, `${pathname} accepted: ${text}`);
  return text ? JSON.parse(text) : {};
}

async function resourceRequest(method: string, pathname: string, body?: unknown): Promise<any> {
  // Public-resource protocol decision rules: Memory/Skill/File routes carry
  // exactly their own beta and configuration routes carry none. Missing or
  // cross-family beta fails before this test's Postgres/Worker causes can run;
  // combining betas would create a false compatibility surface.
  const beta = pathname.startsWith('memory_stores')
    ? MEMORY_BETA
    : pathname.startsWith('skills')
      ? SKILLS_BETA
      : pathname.startsWith('files')
        ? FILES_BETA
        : undefined;
  const response = await fetch(`${CONFIG_BASE}/v1/workspaces/${WORKSPACE}/${pathname}`, {
    method,
    headers: {
      ...(beta === undefined ? {} : { 'anthropic-beta': beta }),
      ...(body === undefined ? {} : { 'content-type': 'application/json' }),
    },
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
    headers: { 'anthropic-beta': FILES_BETA },
    body: form,
  });
  const text = await response.text();
  assert.equal(response.status, 200, `file upload accepted: ${text}`);
  return JSON.parse(text).id;
}

async function configureSeedModel(upstream: string): Promise<void> {
  // The production binary has no test-only echo fallback. One canonical provider
  // connection supplies the seed activation whose immutable request is then
  // specialized for the resource Worker. FMECA: restoring a scenario-only model
  // would bypass production publication; absence must fail before queue mutation.
  const response = await fetch(
    `${CONFIG_BASE}/v1/workspaces/${WORKSPACE}/config/provider-connections`,
    {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({
        idempotency_key: `resource-worker-seed-${process.pid}`,
        workspace_id: WORKSPACE,
        provider_id: 'anthropic',
        display_name: 'Resource Worker Seed',
        dialect: 'anthropic_messages',
        base_url: `${upstream}/v1/`,
        timeout_secs: 30,
        secret: SEED_KEY,
      }),
    },
  );
  const text = await response.text();
  assert.equal(response.status, 201, `seed provider connection: ${text}`);
  const authored = await resourceRequest('PUT', `config/agents/${SEED_AGENT}`, {
    name: SEED_AGENT,
    model: { id: SEED_MODEL },
    system: 'Produce a seed activation only.',
    // Capability/publication decision table. C1 the frozen Skill contains a
    // supporting asset and therefore requires Managed-filesystem delivery;
    // C2 the Agent selects the canonical built-in toolset. C1+C2 => the Worker
    // may realize the exact Skill tree; C1+!C2 => runtime resolution fails
    // closed before sandbox/model effects. Constraint K: the immutable Agent
    // publication, not the resource envelope or Worker, owns executable tool
    // capability. Rule S1=C1+C2=>filesystem delivery; S2=C1+!C2=>reject.
    tools: [{ type: 'agent_toolset_20260401' }],
    max_steps: 1,
  });
  assert.equal(authored.id, SEED_AGENT);
  const published = await resourceRequest('POST', `config/agents/${SEED_AGENT}/publish`);
  assert.equal(published.installed, true);
}

async function uploadSkill(): Promise<{ skill_id: string; version: number; bundle_sha256: string }> {
  const skillMarkdown = Buffer.from(
    `---\nname: ${SKILL_NAME}\ndescription: frozen remote worker Skill\n---\nRead the supporting asset.`,
  );
  const form = new FormData();
  form.append(
    'files[]',
    new Blob([skillMarkdown], { type: 'text/markdown' }),
    'SKILL.md',
  );
  form.append(
    'files[]',
    new Blob([SKILL_BINARY], { type: 'application/octet-stream' }),
    'assets/data.bin',
  );
  const created = await fetch(`${CONFIG_BASE}/v1/workspaces/${WORKSPACE}/skills`, {
    method: 'POST',
    headers: { 'anthropic-beta': SKILLS_BETA },
    body: form,
  });
  const createdText = await created.text();
  assert.equal(created.status, 200, `Skill upload accepted: ${createdText}`);
  const skillId = JSON.parse(createdText).id;
  const version = await fetch(
    `${CONFIG_BASE}/v1/workspaces/${WORKSPACE}/skills/${skillId}/versions/1`,
    { headers: { 'anthropic-beta': SKILLS_BETA } },
  );
  const versionText = await version.text();
  assert.equal(version.status, 200, `Skill version retrieved: ${versionText}`);
  const projected = JSON.parse(versionText);
  const hash = createHash('sha256');
  for (const [filePath, content] of [
    ['SKILL.md', skillMarkdown] as const,
    ['assets/data.bin', SKILL_BINARY] as const,
  ]) {
    const pathBytes = Buffer.from(filePath);
    const pathLength = Buffer.alloc(8);
    pathLength.writeBigUInt64BE(BigInt(pathBytes.length));
    const contentLength = Buffer.alloc(8);
    contentLength.writeBigUInt64BE(BigInt(content.length));
    hash.update(pathLength).update(pathBytes).update(contentLength).update(content);
  }
  return {
    skill_id: skillId,
    version: Number(projected.version),
    bundle_sha256: `sha256:${hash.digest('hex')}`,
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
          // Seed claim decision table: C1 the published candidate requires a
          // shared credential source; C2 the claiming driver reports the exact
          // installed Native realization profile. C1+C2 => claim; C1+!C2 =>
          // ineligible before lease mutation. FMECA: a high-level source flag
          // alone could falsely imply a last-mile adapter, so claim admission
          // also consumes the canonical structured evidence.
          capabilities: [
            'credential-source/v1',
            'host-executor/v1',
            'native-runtime',
            WORKER_PROVIDER_CREDENTIAL_CAPABILITY,
          ],
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
  const inputs: any[] = fileId === undefined
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
  // This fixture exercises the ordinary Run resource envelope, not the
  // SessionApplication realization protocol. `session_thread_id` is the sole
  // contract discriminator for the latter: setting it would correctly require
  // a durable Managed Session with this id and make the handcrafted queue item
  // an invalid parallel Session-creation path.
  //
  // | session_thread_id | Durable Session exists | Effect |
  // |---|---|---|
  // | absent | n/a | ordinary manifest realization |
  // | present | yes | canonical Session control realization |
  // | present | no | fail closed before sandbox/model use |
  delete request.session_thread_id;
  // Raw Provider prerequisite: C0 all route coordinates, including the opaque
  // fixture dialect, are explicit -> E0 ingress admits the resource-bound Run.
  // Constraint/K: this helper supplies no default or duplicate validation;
  // ResolvedModelCandidate deserialization remains the sole validity authority.
  // Rule M0=C0=>E0; malformed-route rejection belongs to worker_transport.
  request.activation.snapshot.resolved_spec.model_binding =
    nativeProviderCandidateFixture({
      binding: request.activation.snapshot.resolved_spec.model_binding,
      providerRef: 'fixture-provider@1',
      routeRef: 'fixture-worker-local@1',
      scopeId: WORKSPACE,
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
      adapterKind: 'fixture',
      apiDialect: 'fixture',
      baseUrl: 'https://worker-local.invalid',
      upstreamModel: request.activation.snapshot.resolved_spec.model_binding.model_ref,
    });
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

async function listDispatches(thread: string): Promise<any[]> {
  const response = await fetch(`${BASE}/v1/durable/threads/${thread}/dispatches`);
  const text = await response.text();
  assert.equal(response.status, 200, `list dispatches for ${thread}: ${text}`);
  return (JSON.parse(text) as any).dispatches ?? [];
}

async function waitUntilSettled(thread: string, timeoutMs = 30_000): Promise<void> {
  await waitForValue(
    () => listDispatches(thread),
    (dispatches: any[]) => dispatches.length === 0,
    `Thread ${thread} dispatches to settle`,
    { timeoutMs, pollMs: 50 },
  );
}

function managedContainerIds(): string[] {
  const output = docker(
    'ps', '-aq',
    '--filter', 'label=awaken.sandbox=1',
    '--filter', `ancestor=${SESSION_IMAGE}`,
  );
  return output ? output.split(/\s+/).filter(Boolean) : [];
}

async function waitForNewContainer(
  before: Set<string>,
  marker: string,
  timeoutMs = 30_000,
): Promise<string> {
  return waitForValue(
    managedContainerIds,
    (ids: string[]) => ids.filter((id: string) => !before.has(id)).length === 1,
    marker,
    { timeoutMs },
  ).then((ids: string[]) => ids.find((id: string) => !before.has(id))!);
}

async function waitForContainerFile(
  container: string,
  file: string,
  expected: Buffer | undefined,
  timeoutMs = 30_000,
): Promise<void> {
  const deadline = Date.now() + timeoutMs;
  let observed = '<missing>';
  while (Date.now() <= deadline) {
    const result = expected === undefined
      ? spawnSync('docker', ['exec', container, 'test', '!', '-e', file])
      : spawnSync('docker', ['exec', container, 'cat', file]);
    if (expected === undefined ? result.status === 0 : result.status === 0 && result.stdout.equals(expected)) {
      return;
    }
    observed = result.status === 0 ? result.stdout.toString('hex') : result.stderr.toString();
    await sleep(50);
  }
  throw new Error(`container projection ${container}:${file} did not converge; observed=${observed}`);
}

async function enqueueAndAwait(request: any, seedWorkerId: string): Promise<void> {
  // Cause/effect rule: ordinary resource requests deliberately omit the
  // Session-realization discriminator, while dispatch settlement is keyed by
  // the activation's authoritative Thread id. Waiting on session_thread_id
  // would poll `undefined` and never observe the already-settled dispatch.
  await post('/v1/worker/dispatch/enqueue', { request }, seedWorkerId);
  await waitUntilSettled(request.activation.thread_id);
}

async function main(): Promise<void> {
  // Test design (remote Worker resource manifest). Causes: C1=the claimed Run
  // carries frozen File/Skill/Memory inputs and exact Workspace; C2=attach/detach
  // operations succeed or fail; C3=Workspace mismatches or Memory is archived;
  // C4=Worker capability/placement satisfies the request. Effects: E1=valid
  // inputs plus the Agent's explicit filesystem toolset materialize exact
  // bytes/tree/mount in one sandbox; E2=detach removes
  // only that projection; E3=C3/C4-invalid remains retryable and creates no
  // sandbox/model effect. Constraints/invariant: the dispatch activation plus
  // frozen resource envelope is the sole Worker authority; control-plane lookup
  // cannot widen it. Decision rules: M1=C1+C2+C4=>E1+E2;
  // M2=C1+C3=>E3; M3=C1+!C4=>E3. A selected filesystem-backed Skill with no
  // filesystem tool is the separate fail-closed S2 rule beside its authoring
  // fixture; this positive scenario must not acquire capability from resources.
  const database = await postgres();
  const upstream = await startFakeAnthropic(SEED_KEY, { models: [SEED_MODEL] });
  const configStorage = mkdtempSync(path.join(tmpdir(), 'awaken-resource-config-'));
  const workerStorage = mkdtempSync(path.join(tmpdir(), 'awaken-resource-worker-'));
  // Seed-model decision table: R1 explicit scenario model -> the production
  // management composition emits one real serialized activation for this
  // resource-boundary test; R2 production with no configured model -> durable
  // submission fails closed (covered by the no-model Host tests), never an
  // implicit echo fallback. Resource/Postgres/dispatch/Worker adapters remain
  // the production implementations in both rules.
  const management = spawnProduction(
    configStorage,
    PORT,
    {
      // Management-plane identity decision table for this resource-boundary
      // scenario: no-login + no Authorization => exercise the resource
      // contract; embedded/cloud IAM + no Authorization => reject before
      // multipart ingestion. Authentication behavior has its own E2Es.
      identityMode: 'no-login',
      controlSealKey:
        '00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff',
      databases: {
        resource_database_url: database.url,
        admin_db: database.url,
        sessions_db: database.url,
        runtime_database_url: database.url,
      },
      // Cause/effect decision table for the split transport boundary:
      // C1 run_local_pool=false + C2 internal_bind present -> the public API
      // owns resource authoring while authenticated Worker/dispatch traffic uses
      // the private listener. C1 + !C2 -> startup fails closed. FMECA: merging
      // listeners exposes internal mutation routes; omitting C2 strands remote
      // work. Separate bases plus real Worker completion detect both failures.
      fields: {
        run_local_pool: false,
        internal_bind: `127.0.0.1:${INTERNAL_PORT}`,
        // Seed-selection decision table: both registered Workers satisfy the
        // production-authored seed and have zero in-flight work; the canonical
        // least-loaded policy then orders exact WorkerIdentity values. A remote
        // `resource-seed-*` identity sorts before this explicit `zz-*` embedded
        // identity and therefore captures the wire deterministically. FMECA:
        // leaving the product-default `awaken-worker` id would win the tie and
        // settle the seed before the remote fixture can inspect it.
        worker_id: `zz-resource-embedded-${process.pid}`,
      },
      // The production process owns execution topology through the typed
      // SESSION_DEPLOYMENT_* fixture boundary, while spawnProduction owns the
      // one config.toml source. Both name the same Postgres cell; otherwise the
      // Coordinator rejects an unrooted volatile SQLite dispatch authority.
      extraEnv: {
        SESSION_DEPLOYMENT_INGRESS: 'durable',
        SESSION_DEPLOYMENT_STORAGE_DIR: configStorage,
        SESSION_DEPLOYMENT_DATABASE_URL: database.url,
        SESSION_DEPLOYMENT_DISPATCH_BACKEND: 'postgres',
        SESSION_DEPLOYMENT_STORE: 'postgres',
      },
    },
  );
  let worker: ChildProcessWithoutNullStreams | undefined;
  let workerOutput = '';
  const ownedSandboxContainers = new Set<string>();
  try {
    await waitForPort(PORT, 180_000, management);
    await waitForPort(INTERNAL_PORT, 180_000, management);
    WORKSPACE = fs.readFileSync(
      path.join(configStorage, 'platform-workspace-id'),
      'utf8',
    ).trim();
    await configureSeedModel(upstream.url);
    const fileId = await uploadFile();
    const skill = await uploadSkill();
    const memory = await createMemory();
    const seedWorker = await registerSeedWorker();

    await post(`/v1/durable/threads/${THREAD}-seed/submit_background`, {
      agent: SEED_AGENT,
      text: 'seed activation',
    });
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

    // Cause/effect decision rule for the seed handoff: C_seed the current
    // owner/epoch commits this exact seed Run as Ended -> E_seed its durable
    // receipt precedes terminal observation and Done removal. Constraint K:
    // enqueueing the cloned resource Run is not committed truth for the seed;
    // missing/mismatched/nonterminal evidence stays owned by the Rust
    // settlement decision table.
    const seedCommit = claimedCommitRequestFixture({
      claimed: seedClaim,
      commit: terminalThreadCommitFixture({
        runId: seedClaim.lease.run_id,
        threadId: seedClaim.request.activation.thread_id,
        messageId: `seed-terminal-${seedClaim.lease.run_id}`,
        text: 'seed ownership completed before resource worker handoff',
      }),
      ordinal: 0,
      expectedThreadVersion: 0,
    });
    const seedCommitted = await post(
      '/v1/worker/commit-claimed',
      { ...seedCommit, identity: seedWorker.identity },
      seedWorker.id,
    );
    assert.ok(
      typeof seedCommitted.commit_sequence === 'number',
      'E_seed durable seed receipt precedes Done settlement',
    );
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
    const preexistingSandboxes = new Set(managedContainerIds());
    Object.assign(env, {
      AWAKEN_UPSTREAM_URL: INTERNAL_BASE,
      SESSION_DEPLOYMENT_INGRESS: 'durable',
      SESSION_DEPLOYMENT_STORAGE_DIR: workerStorage,
      // This scenario requires enforced read-only mounts. Select the available
      // production Docker tier explicitly; the namespace default must not
      // silently degrade when bwrap is unavailable.
      AWAKEN_TEST_SANDBOX_TIER: 'docker',
      AWAKEN_TEST_CONTAINER_IMAGE: SESSION_IMAGE,
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
    const sandboxContainer = await waitForNewContainer(
      preexistingSandboxes,
      'the first frozen manifest owns one opaque sandbox container',
    );
    ownedSandboxContainers.add(sandboxContainer);
    // File path decision rule: a requested path already rooted below
    // `mnt/session/uploads` is preserved; every other safe logical path is
    // projected below that public root. `uploads/input.txt` therefore proves
    // the latter as `/mnt/session/uploads/uploads/input.txt`.
    const projectedFile = `/mnt/session/uploads/${MOUNT_PATH}`;
    const projectedSkill = `/workspace/.skills/${skill.skill_id}/assets/data.bin`;
    await waitForContainerFile(sandboxContainer, projectedFile, FILE_BYTES);
    await waitForContainerFile(sandboxContainer, projectedSkill, SKILL_BINARY);

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
    await waitForContainerFile(sandboxContainer, projectedFile, undefined);
    await waitForContainerFile(sandboxContainer, '/workspace/.skills', undefined);

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
    await waitForContainerFile(sandboxContainer, projectedFile, FILE_BYTES);
    await waitForContainerFile(sandboxContainer, projectedSkill, SKILL_BINARY);
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
    const containersBeforeMemory = new Set(managedContainerIds());
    await enqueueAndAwait(memoryRequest, seedWorker.id);
    const memoryContainer = await waitForNewContainer(
      containersBeforeMemory,
      'Memory projection creates one additional opaque sandbox container',
    );
    ownedSandboxContainers.add(memoryContainer);
    const projectedMemory = '/mnt/memory/fact.md';
    await waitForContainerFile(memoryContainer, projectedMemory, MEMORY_BYTES);

    // Workspace mismatch decision rule: C1 exact Run is claimed with a manifest
    // outside its execution scope -> E1 the Worker emits that exact rejection
    // before sandbox creation. A transient Leased projection is not an oracle:
    // the retryable dispatch may return to Pending between public reads.
    // Constraints/invariant: activation Workspace fences every resource id and
    // the rejected Run retains custody without acquiring a sandbox.
    const foreignThread = `${THREAD}-foreign`;
    const foreign = runRequest(
      seedClaim.request,
      'foreign',
      resourceEnvelope(fileId, `${WORKSPACE}-other`, skill),
    );
    foreign.activation.thread_id = foreignThread;
    const containersBeforeForeign = managedContainerIds();
    await post('/v1/worker/dispatch/enqueue', { request: foreign }, seedWorker.id);
    await waitForValue(
      () => workerOutput,
      (output: string) => output.includes(foreign.activation.run_id)
        && output.includes('resource manifest outside its execution scope'),
      `Worker to reject exact cross-Workspace Run ${foreign.activation.run_id}`,
      { timeoutMs: 30_000, pollMs: 50 },
    );
    assert.deepEqual(
      managedContainerIds(),
      containersBeforeForeign,
      'scope-mismatched resource dispatch failed before sandbox creation',
    );
    // Live-state decision rule: C1 immutable Memory config remains pinned + C2
    // live store is Archived => E1 its retained exact Run is attempted and the
    // pinned store is rejected as inactive, E2 no new sandbox/model effect.
    // Pending after the rejection retains retryable custody; a fleeting Leased
    // projection is deliberately ineligible.
    await resourceRequest('POST', `memory_stores/${memory.memory_store_id}/archive`);
    const deniedMemory = runRequest(
      seedClaim.request,
      'memory-archived',
      resourceEnvelope(undefined, WORKSPACE, undefined, memory),
      MEMORY_THREAD,
    );
    const containersBeforeDeniedMemory = managedContainerIds();
    const requestsBeforeDeniedMemory = upstream.requests.length;
    const outputFence = workerOutput.length;
    await post('/v1/worker/dispatch/enqueue', { request: deniedMemory }, seedWorker.id);
    await waitForValue(
      () => workerOutput.slice(outputFence),
      (output: string) => output.includes(memory.memory_store_id)
        && output.includes('not active (Archived)'),
      `Worker to reject archived Memory after Run ${deniedMemory.activation.run_id} admission`,
      { timeoutMs: 30_000, pollMs: 50 },
    );
    await waitForValue(
      () => listDispatches(MEMORY_THREAD),
      (dispatches: any[]) => dispatches.some(
        (dispatch) => dispatch.run_id === deniedMemory.activation.run_id
          && dispatch.status === 'Pending',
      ),
      `archived-Memory Run ${deniedMemory.activation.run_id} to retain retryable custody`,
      { timeoutMs: 30_000, pollMs: 50 },
    );
    assert.deepEqual(managedContainerIds(), containersBeforeDeniedMemory, 'E2: no sandbox effect');
    assert.equal(upstream.requests.length, requestsBeforeDeniedMemory, 'E2: no model effect');

    assert.ok(!workerOutput.includes(FILE_BYTES.toString()), 'worker logs do not expose File bytes');
    console.log(
      'WORKER RESOURCE MANIFEST TS E2E PASS: frozen File/Skill/Memory realization, exact-tree detach, live Memory deny, capability placement, and cross-Workspace failure crossed real cell/worker processes.',
    );
  } finally {
    if (worker) await stopServer(worker).catch(() => {});
    const remainingOwned = [...ownedSandboxContainers].filter((id) =>
      spawnSync('docker', ['container', 'inspect', id], { stdio: 'ignore' }).status === 0
    );
    if (remainingOwned.length > 0) {
      spawnSync('docker', ['rm', '-f', ...remainingOwned], { stdio: 'ignore' });
    }
    await stopServer(management).catch(() => {});
    upstream.close();
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
