// T30-C02 (SDK 到真实本地服务): the official TypeScript SDK (@anthropic-ai/sdk,
// e2e lock) and the official Python SDK (anthropic, locked in
// packages/managed-sdk-oracle/python/requirements.lock, fresh venv) drive the
// SAME production `awaken` process through the same Environment / Memory /
// Session lifecycle, pagination and error rows.
//
// Oracles (groups-5): O0 the requests really cross the router, authorization
// and application (server-issued request ids, durable resources visible to an
// independent raw client and to the other SDK); O1 original identities,
// versions and errors are returned; O2 both SDKs observe the same thing.
//
// | Row | Check |
// |---|---|
// | D1 | each SDK's normalized lifecycle transcript (versions, ordering, statuses, error classes/types) is identical |
// | D2 | each SDK reads the store the OTHER SDK created: identical ids, paths, hashes, version ids/operations |
// | D3 | O0: every create carries a distinct server `req_` id; every created resource is readable through raw HTTP |
// | NC | a fetch-intercepting TS client returning `data: []` / invented ids fails the D3 check |
//
// Run: build the production binary (harness ensureProductionBuilt / AWAKEN_BIN),
// Python >= 3.10 with venv and access to the locked wheels, then
//   E2E_PORT=<free port> node e2e/sdk_differential_e2e.mjs
import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import Anthropic from '@anthropic-ai/sdk';
import { VERSION as TS_SDK_VERSION } from '@anthropic-ai/sdk/version';
import {
  spawnProduction, stopServer, waitForPort, provisionProviderTestModel, cleanupFixtureTree, pass,
} from './harness.mjs';
import { withoutAmbientProviderKeys } from './process_environment.mjs';
import { startFakeAnthropic } from './fixtures/fake_anthropic_fixture.mjs';

const REPO = path.resolve(import.meta.dirname, '..');
const LOCK = path.join(REPO, 'packages/managed-sdk-oracle/python/requirements.lock');
const DRIVER = path.join(import.meta.dirname, 'sdk_differential_driver.py');
const PORT = Number(process.env.E2E_PORT ?? 38634);
const BASE = `http://127.0.0.1:${PORT}`;
const MANAGED_BETA = 'managed-agents-2026-04-01';
const MEMORY_BETA = 'agent-memory-2026-07-22';
const BETAS = [MANAGED_BETA];
// Memory-store endpoints take the agent-memory beta alone: it replaces the
// managed-agents beta there and the server refuses both together.
const MEMORY_BETAS = [MEMORY_BETA];
const REQUEST_ID = /^req_[0-9a-f]{32}$/u;

const directory = fs.mkdtempSync(path.join(os.tmpdir(), 'awaken-t30c02-'));
const data = path.join(directory, 'server-data');
let server;
let upstream;

// ---- the Python half ------------------------------------------------------------
function provisionPython() {
  const candidates = process.env.AWAKEN_PYTHON_SDK_PYTHON
    ? [process.env.AWAKEN_PYTHON_SDK_PYTHON]
    : ['python', 'python3', 'python3.13', 'python3.12', 'python3.11', 'python3.10'];
  for (const candidate of candidates) {
    const probe = spawnSync(candidate, ['-c', 'import sys; raise SystemExit(sys.version_info < (3, 10))']);
    if (probe.error || probe.status !== 0) continue;
    const venv = path.join(directory, 'venv');
    if (spawnSync(candidate, ['-m', 'venv', venv]).status !== 0) continue;
    const bin = process.platform === 'win32' ? 'Scripts' : 'bin';
    const python = path.join(venv, bin, process.platform === 'win32' ? 'python.exe' : 'python');
    const install = spawnSync(python, ['-m', 'pip', 'install', '--disable-pip-version-check', '--no-input',
      '--requirement', LOCK], { stdio: 'inherit' });
    assert.equal(install.status, 0, 'install the locked Python SDK closure');
    return python;
  }
  throw new Error('T30-C02 requires Python >= 3.10 with venv (set AWAKEN_PYTHON_SDK_PYTHON)');
}

function python(interpreter, args) {
  const result = spawnSync(interpreter, [DRIVER, ...args], {
    env: { ...process.env, AWAKEN_MANAGED_BASE_URL: BASE },
    encoding: 'utf8',
    timeout: 180_000,
  });
  assert.equal(result.status, 0, `python ${args.join(' ')} failed:\n${result.stderr}`);
  return JSON.parse(result.stdout.trim().split('\n').at(-1));
}

// ---- the TypeScript half (mirrors sdk_differential_driver.py) -------------------
async function errorRow(call) {
  try {
    await call();
  } catch (error) {
    if (!(error instanceof Anthropic.APIError)) throw error;
    const requestId = error.requestID ?? error.request_id ?? error.headers?.get?.('request-id');
    return {
      class: error.constructor.name,
      status: error.status,
      type: error.error?.error?.type ?? null,
      request_id: Boolean(requestId && String(requestId).startsWith('req_')),
    };
  }
  return { class: null };
}

async function drain(page) {
  const items = [];
  for await (const item of page) items.push(item);
  return items;
}

async function lifecycleTs(client, label, model) {
  const beta = client.beta;
  const requestIds = [];
  const environment = await beta.environments.create({ name: `t30-${label}-env`, config: { type: 'self_hosted' }, betas: BETAS });
  requestIds.push(environment._request_id);
  const retrieved = await beta.environments.retrieve(environment.id, { betas: BETAS });
  const updated = await beta.environments.update(environment.id, { description: 'updated by the lifecycle', betas: BETAS });
  const archived = await beta.environments.archive(environment.id, { betas: BETAS });

  const store = await beta.memoryStores.create({ name: `t30-${label}-store`, betas: MEMORY_BETAS });
  requestIds.push(store._request_id);
  const memories = [];
  for (let index = 0; index < 3; index += 1) {
    const memory = await beta.memoryStores.memories.create(store.id, { path: `/p${index}.md`, content: `c${index}`, betas: MEMORY_BETAS });
    requestIds.push(memory._request_id);
    memories.push(memory);
  }
  const precondition = { type: 'content_sha256', content_sha256: memories[0].content_sha256 };
  await beta.memoryStores.memories.update(memories[0].id, { memory_store_id: store.id, content: 'c0-v2', precondition, betas: MEMORY_BETAS });
  const stale = await errorRow(() => beta.memoryStores.memories.update(memories[0].id, {
    memory_store_id: store.id, content: 'stale', precondition, betas: MEMORY_BETAS,
  }));
  const versions = await drain(beta.memoryStores.memoryVersions.list(store.id, { limit: 2, betas: MEMORY_BETAS }));
  const listed = await drain(beta.memoryStores.memories.list(store.id, { limit: 2, betas: MEMORY_BETAS }));
  const ordinal = new Map(memories.map((memory, index) => [memory.id, index]));

  const agent = await beta.agents.create({ name: `t30-${label}-agent`, model, system: 't30 differential', betas: BETAS });
  requestIds.push(agent._request_id);
  const session = await beta.sessions.create({ agent: agent.id, environment_id: 'env_local', betas: BETAS });
  requestIds.push(session._request_id);
  const sameSession = (await beta.sessions.retrieve(session.id, { betas: BETAS })).id === session.id;
  await beta.sessions.delete(session.id, { betas: BETAS });

  return {
    normalized: {
      environment: {
        retrieved_same: retrieved.id === environment.id,
        description: updated.description,
        archived: archived.archived_at !== null && archived.archived_at !== undefined,
      },
      memory: {
        versions: versions.map((v) => [v.operation, v.path, ordinal.get(v.memory_id) ?? null]),
        listed_paths: listed.map((m) => m.path),
        stale_update: stale,
      },
      session: {
        retrieved_same: sameSession,
        status: session.status,
        after_delete: await errorRow(() => beta.sessions.retrieve(session.id, { betas: BETAS })),
      },
      errors: {
        missing_environment: await errorRow(() => beta.environments.retrieve('env_t30_does_not_exist', { betas: BETAS })),
        zero_page_limit: await errorRow(() => beta.sessions.list({ limit: 0, betas: BETAS })),
      },
    },
    ids: { store: store.id, environment: environment.id, agent: agent.id },
    request_ids: requestIds,
  };
}

async function observeTs(client, storeId) {
  const beta = client.beta;
  const store = await beta.memoryStores.retrieve(storeId, { betas: MEMORY_BETAS });
  return {
    store: [store.id, store.name],
    memories: (await drain(beta.memoryStores.memories.list(storeId, { limit: 2, betas: MEMORY_BETAS })))
      .map((m) => [m.id, m.path, m.content_sha256]),
    versions: (await drain(beta.memoryStores.memoryVersions.list(storeId, { limit: 2, betas: MEMORY_BETAS })))
      .map((v) => [v.id, v.operation, v.memory_id, v.path]),
  };
}

// ---- O0: did the requests reach the real router/authorization/application? -----
async function routerViolations(label, observation) {
  const violations = [];
  const ids = observation.request_ids ?? [];
  if (ids.length === 0) violations.push(`${label}/O0 no request id was observed`);
  for (const id of ids) {
    if (!REQUEST_ID.test(String(id))) violations.push(`${label}/O0 ${id} is not a server-issued request id`);
  }
  if (new Set(ids).size !== ids.length) violations.push(`${label}/O0 request ids repeat`);
  const headers = (betas) => ({ 'x-api-key': 'e2e-local', 'anthropic-version': '2023-06-01', 'anthropic-beta': betas.join(',') }); // awaken-allow(secret): (local no-login placeholder)
  for (const [route, id, betas] of [['memory_stores', observation.ids.store, MEMORY_BETAS], ['agents', observation.ids.agent, BETAS]]) {
    const response = await fetch(`${BASE}/v1/${route}/${id}`, { headers: headers(betas) });
    if (response.status !== 200) violations.push(`${label}/O0 ${route}/${id} is not durable on the server (${response.status})`);
    else if (!REQUEST_ID.test(response.headers.get('request-id') ?? '')) violations.push(`${label}/O0 raw read has no request id`);
  }
  return violations;
}

async function main() {
  assert.ok(Number.isInteger(PORT) && PORT > 0, 'E2E_PORT');
  const interpreter = provisionPython();
  const key = 't30c02-fixture-key'; // awaken-allow(secret): (synthetic upstream)
  upstream = await startFakeAnthropic(key, { models: ['t30c02-fixture'] });
  server = spawnProduction(data, PORT, { fields: { log_filter: 'warn' } }, withoutAmbientProviderKeys(process.env));
  await waitForPort(PORT, 120_000, server);
  const context = await fetch(`${BASE}/v1/config/workspace-context`);
  assert.ok(context.ok, 'workspace context');
  const published = await provisionProviderTestModel(BASE, context.headers.get('anthropic-workspace-id'), {
    id: 't30c02', provider_id: 'anthropic', dialect: 'anthropic_messages', base_url: `${upstream.url}/v1`,
    secret: key, model_id: 't30c02-fixture', timeout_secs: 60,
  });
  const model = typeof published.model === 'string' ? published.model : JSON.stringify(published.model);
  const client = new Anthropic({ apiKey: 'e2e-local', baseURL: BASE, maxRetries: 0 }); // awaken-allow(secret): (local no-login placeholder)

  // D1: the same lifecycle through each SDK.
  const ts = await lifecycleTs(client, 'ts', model);
  const py = python(interpreter, ['lifecycle', 'py', model]);
  assert.deepEqual(py.normalized, ts.normalized, 'O1/O2 D1: both SDKs observe the same lifecycle');
  assert.equal(ts.normalized.memory.stale_update.class, 'ConflictError', 'O1 a stale precondition is a conflict');
  assert.equal(ts.normalized.session.after_delete.class, 'NotFoundError', 'O1 a deleted Session is not found');

  // D3: both halves really reached the server.
  assert.deepEqual(await routerViolations('ts', ts), [], 'O0 TypeScript requests reached the real service');
  assert.deepEqual(await routerViolations('python', py), [], 'O0 Python requests reached the real service');
  assert.equal(new Set([...ts.request_ids, ...py.request_ids]).size, ts.request_ids.length + py.request_ids.length,
    'O0 every request was issued its own server id');

  // D2: each SDK reads the store the other one created.
  for (const storeId of [ts.ids.store, py.ids.store]) {
    const pyView = python(interpreter, ['observe', storeId]);
    const tsView = await observeTs(client, storeId);
    assert.deepEqual(pyView.store, tsView.store, 'O2 D2 store identity');
    assert.deepEqual(pyView.memories, tsView.memories, 'O1/O2 D2 identical memory ids, paths and hashes');
    assert.deepEqual(pyView.versions, tsView.versions, 'O1/O2 D2 identical version ids and operations');
  }
  pass(`T30-C02: TS ${TS_SDK_VERSION} and Python ${py.sdk.version} agree on one production server`);

  // NC: a client whose fetch never reaches the server.
  const stubbed = new Anthropic({
    apiKey: 'e2e-local', // awaken-allow(secret): (local no-login placeholder)
    baseURL: BASE,
    maxRetries: 0,
    fetch: async (url) => new Response(JSON.stringify(String(url).includes('/memories')
      ? { data: [], next_page: null }
      : { id: 'memstore_stubbed', type: 'memory_store', name: 'stub' }), {
      status: 200, headers: { 'content-type': 'application/json' },
    }),
  });
  const stubStore = await stubbed.beta.memoryStores.create({ name: 'stub', betas: MEMORY_BETAS });
  const stubListed = await drain(stubbed.beta.memoryStores.memories.list(stubStore.id, { limit: 2, betas: MEMORY_BETAS }));
  assert.deepEqual(stubListed, [], 'the stub answers data: []');
  const violations = await routerViolations('stub', {
    request_ids: [stubStore._request_id],
    ids: { store: stubStore.id, agent: 'agent_stubbed' },
  });
  assert.ok(violations.some((v) => v.includes('not a server-issued request id')), `NC request id: ${violations}`);
  assert.ok(violations.some((v) => v.includes('is not durable on the server')), `NC durability: ${violations}`);
  pass('T30-C02 NC: an intercepted fetch is not counted as the real service');
}

try {
  await main();
  console.log('\nE2E PASS: T30-C02 TypeScript and Python SDKs against one real service');
} catch (error) {
  console.error(`E2E FAIL: ${error.stack}`);
  process.exitCode = 1;
} finally {
  if (server) await stopServer(server);
  if (upstream) await upstream.close();
  cleanupFixtureTree(directory);
}
