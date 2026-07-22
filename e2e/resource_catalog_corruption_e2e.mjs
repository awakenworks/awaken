// Fail-closed E2E for corrupt Resource Catalog aggregates. A definition whose
// current immutable config is missing must never fall back to defaults/current
// remote state. Restoring the same catalog data lets the persisted activation
// converge without changing its Session snapshot.

import assert from 'node:assert/strict';
import fs from 'node:fs';
import net from 'node:net';
import os from 'node:os';
import path from 'node:path';
import { execFileSync, execSync, spawn } from 'node:child_process';
import { fileURLToPath } from 'node:url';

const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const PORT = Number(process.env.E2E_PORT ?? 38439);
const WORKSPACE = `catalog-corruption-${process.pid}`;
const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

function binary() {
  const output = execSync('cargo build --quiet --message-format=json -p awaken-cli --bin awaken', {
    cwd: ROOT,
    maxBuffer: 64 * 1024 * 1024,
  }).toString();
  for (const line of output.split('\n')) {
    try {
      const message = JSON.parse(line);
      if (message.executable && message.target?.name === 'awaken') return message.executable;
    } catch { /* cargo diagnostic */ }
  }
  throw new Error('awaken binary was not produced');
}

function start(bin, directory) {
  return spawn(bin, {
    env: {
      ...process.env,
      AWAKEN_HTTP_ADDR: `127.0.0.1:${PORT}`,
      AWAKEN_LOCAL_WORKSPACE_ID: WORKSPACE,
      AWAKEN_STORAGE_DIR: directory,
      AWAKEN_DEPLOYMENT_DATA_DIR: directory,
      AWAKEN_MGMT_SEAL_KEY: '00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff',
    },
    stdio: ['ignore', 'ignore', 'inherit'],
  });
}

async function ready(child) {
  const deadline = Date.now() + 60_000;
  while (Date.now() < deadline) {
    const connected = await new Promise((resolve) => {
      const socket = net.createConnection({ port: PORT, host: '127.0.0.1' });
      socket.once('connect', () => { socket.destroy(); resolve(true); });
      socket.once('error', () => { socket.destroy(); resolve(false); });
    });
    if (connected) return;
    if (child.exitCode !== null) throw new Error(`awaken exited with ${child.exitCode}`);
    await sleep(100);
  }
  throw new Error('awaken did not become ready');
}

async function stop(child, signal = 'SIGINT') {
  if (child.exitCode !== null) return;
  child.kill(signal);
  await new Promise((resolve) => child.once('exit', resolve));
}

const scoped = (tail) =>
  `http://127.0.0.1:${PORT}/v1/workspaces/${WORKSPACE}/${tail}`;

async function json(method, tail, body) {
  const response = await fetch(scoped(tail), {
    method,
    headers: body === undefined ? {} : { 'content-type': 'application/json' },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  const text = await response.text();
  return { status: response.status, body: text ? JSON.parse(text) : null };
}

function seedRepository(root) {
  const work = path.join(root, 'catalog-repository-work');
  const remote = path.join(root, 'catalog-repository.git');
  fs.mkdirSync(work, { recursive: true });
  execFileSync('git', ['init', '-q', '-b', 'main'], { cwd: work });
  execFileSync('git', ['config', 'user.email', 'catalog@example.invalid'], { cwd: work });
  execFileSync('git', ['config', 'user.name', 'catalog-corruption'], { cwd: work });
  fs.writeFileSync(path.join(work, 'README.md'), 'catalog corruption recovery');
  execFileSync('git', ['add', 'README.md'], { cwd: work });
  execFileSync('git', ['commit', '-q', '-m', 'seed'], { cwd: work });
  execFileSync('git', ['clone', '-q', '--bare', work, remote]);
  return remote;
}

function sqlQuote(value) {
  return `'${String(value).replaceAll("'", "''")}'`;
}

function catalogRecord(database, kind, id) {
  const output = execFileSync('sqlite3', [
    '-json',
    database,
    `SELECT data FROM admin_resource_catalog WHERE kind=${sqlQuote(kind)} AND id=${sqlQuote(id)}`,
  ]).toString().trim();
  const rows = output ? JSON.parse(output) : [];
  assert.equal(rows.length, 1, `missing ${kind}/${id} catalog aggregate`);
  return JSON.parse(rows[0].data);
}

function writeCatalogRecord(database, kind, id, record) {
  writeCatalogRaw(database, kind, id, JSON.stringify(record));
}

function writeCatalogRaw(database, kind, id, data) {
  execFileSync('sqlite3', [
    database,
    `UPDATE admin_resource_catalog SET data=${sqlQuote(data)} WHERE kind=${
      sqlQuote(kind)
    } AND id=${sqlQuote(id)}`,
  ]);
}

function sessionResources(database, sessionId) {
  const output = execFileSync('sqlite3', [
    '-json',
    database,
    `SELECT effective_inputs_json FROM managed_session WHERE session_id=${sqlQuote(sessionId)}`,
  ]).toString().trim();
  const rows = output ? JSON.parse(output) : [];
  assert.equal(rows.length, 1, `missing Session ${sessionId}`);
  return JSON.parse(rows[0].effective_inputs_json);
}

function persistPreparedGeneration(database, sessionId) {
  const state = sessionResources(database, sessionId);
  const revision = state.revision + 1;
  const previous = state.activations.map((activation) => ({
    ...activation,
    state: activation.state === 'active' ? 'releasing' : activation.state,
  }));
  const prepared = state.activations
    .filter((activation) => activation.state === 'active')
    .map((activation) => ({
      ...activation,
      activation_id: `${sessionId}:${revision}:${activation.binding_id}`,
      revision,
      state: 'prepared',
      attempts: 0,
    }));
  assert.ok(prepared.length > 0);
  const next = {
    revision,
    active: state.active,
    pending: state.active,
    activations: [...previous, ...prepared],
  };
  execFileSync('sqlite3', [
    database,
    `UPDATE managed_session SET effective_inputs_json=${sqlQuote(JSON.stringify(next))} WHERE session_id=${sqlQuote(sessionId)}`,
  ]);
}

async function main() {
  const directory = fs.mkdtempSync(path.join(os.tmpdir(), 'awaken-catalog-corruption-'));
  const adminDatabase = path.join(directory, 'admin.db');
  const sessionsDatabase = path.join(directory, 'sessions.db');
  const bin = binary();
  let server = start(bin, directory);
  try {
    await ready(server);
    const memory = await json('POST', 'memory_stores', {
      name: 'catalog-fail-closed',
      description: 'configuration must never be inferred',
    });
    assert.equal(memory.status, 200, JSON.stringify(memory.body));

    const repository = seedRepository(directory);
    const session = await json('POST', 'sessions', {
      agent: 'assistant',
      environment_id: 'env_local',
      resources: [{
        type: 'github_repository',
        url: repository,
        mount_path: '/workspace/catalog-repository',
      }],
    });
    assert.equal(session.status, 200, JSON.stringify(session.body));
    const repositoryId = `managed:${session.body.id}:repository:0`;

    await stop(server, 'SIGKILL');
    const memoryRecord = catalogRecord(adminDatabase, 'memory_store', memory.body.id);
    const repositoryRecord = catalogRecord(adminDatabase, 'repository', repositoryId);
    writeCatalogRecord(adminDatabase, 'memory_store', memory.body.id, {
      ...memoryRecord,
      configs: {},
    });
    writeCatalogRecord(adminDatabase, 'repository', repositoryId, {
      ...repositoryRecord,
      configs: {},
    });
    persistPreparedGeneration(sessionsDatabase, session.body.id);

    server = start(bin, directory);
    await ready(server);

    assert.equal((await json('GET', `memory_stores/${memory.body.id}/config`)).status, 500);
    assert.equal(
      (await json('GET', `memory_stores/${memory.body.id}/config_versions/1`)).status,
      500,
    );
    assert.equal(
      (await json('POST', `memory_stores/${memory.body.id}/config`, {
        expected_config_version: 1,
        recall_policy: { enabled: true },
      })).status,
      500,
    );
    assert.equal((await json('DELETE', `memory_stores/${memory.body.id}`)).status, 500);
    const deniedMemoryBinding = await json('POST', 'sessions', {
      agent: 'assistant',
      resources: [{
        type: 'memory_store',
        memory_store_id: memory.body.id,
        mount_path: '/workspace/memory',
      }],
    });
    assert.equal(deniedMemoryBinding.status, 400);
    assert.match(JSON.stringify(deniedMemoryBinding.body), /current config version is missing/u);

    const deniedRepository = sessionResources(sessionsDatabase, session.body.id);
    assert.notEqual(deniedRepository.pending, undefined);
    assert.equal(deniedRepository.activations.at(-1).state, 'prepared');
    assert.equal(deniedRepository.activations.at(-1).attempts, 1);
    assert.match(
      deniedRepository.activations.at(-1).last_error,
      /current config version is missing/u,
    );

    // Repair only the missing immutable histories. The already-persisted Session
    // generation remains unchanged and must be the generation that later commits.
    await stop(server, 'SIGKILL');
    writeCatalogRecord(adminDatabase, 'memory_store', memory.body.id, memoryRecord);
    writeCatalogRecord(adminDatabase, 'repository', repositoryId, repositoryRecord);
    server = start(bin, directory);
    await ready(server);

    const recovered = sessionResources(sessionsDatabase, session.body.id);
    assert.equal(recovered.pending, undefined);
    assert.equal(recovered.activations.at(-1).state, 'active');
    assert.equal(recovered.activations.at(-1).attempts, 2);
    assert.equal(recovered.activations.at(-1).last_error, undefined);
    assert.equal((await json('GET', `memory_stores/${memory.body.id}/config`)).status, 200);

    // Every catalog read validates the complete aggregate. Corrupt durable JSON
    // must fail closed on a cold process without panicking or serving a partial
    // definition/config history. This drives the production collection API so
    // the storage failure remains distinguishable from an ordinary 404.
    await stop(server, 'SIGKILL');
    const memoryConfig = memoryRecord.configs['1'];
    const memoryCorruptions = [
      ['malformed-json', '{not-json'],
      ['forged-definition-id', JSON.stringify({
        ...memoryRecord,
        definition: { ...memoryRecord.definition, id: 'forged-memory-id' },
      })],
      ['empty-workspace', JSON.stringify({
        ...memoryRecord,
        definition: { ...memoryRecord.definition, workspace_id: ' ' },
      })],
      ['zero-current-version', JSON.stringify({
        ...memoryRecord,
        definition: { ...memoryRecord.definition, current_config_version: 0 },
      })],
      ['missing-current-version', JSON.stringify({ ...memoryRecord, configs: {} })],
      ['forged-config-id', JSON.stringify({
        ...memoryRecord,
        configs: { 1: { ...memoryConfig, memory_store_id: 'forged-memory-id' } },
      })],
      ['forged-config-version', JSON.stringify({
        ...memoryRecord,
        configs: { 1: { ...memoryConfig, version: 2 } },
      })],
    ];
    for (const [name, data] of memoryCorruptions) {
      writeCatalogRaw(adminDatabase, 'memory_store', memory.body.id, data);
      server = start(bin, directory);
      await ready(server);
      const denied = await json('GET', 'memory_stores');
      assert.equal(denied.status, 500, `${name}: ${JSON.stringify(denied.body)}`);
      assert.match(JSON.stringify(denied.body), /resource catalog storage failure/u);
      assert.equal(server.exitCode, null, `${name}: catalog corruption crashed the process`);
      await stop(server, 'SIGKILL');
    }
    writeCatalogRecord(adminDatabase, 'memory_store', memory.body.id, memoryRecord);
    server = start(bin, directory);
    await ready(server);
    assert.equal((await json('GET', 'memory_stores')).status, 200);

    console.log('E2E PASS: missing resource configs fail closed and the same snapshot later recovers.');
  } finally {
    await stop(server).catch(() => {});
    fs.rmSync(directory, { recursive: true, force: true });
  }
}

main().catch((error) => {
  console.error('E2E FAIL:', error);
  process.exitCode = 1;
});
