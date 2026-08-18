// Fail-closed E2E for corrupt Resource Catalog aggregates. A definition whose
// current immutable config is missing must never fall back to defaults/current
// remote state. Restoring the same catalog data lets the persisted activation
// converge without changing its Session snapshot.

import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { execFileSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import { spawnProduction, stopServer, waitForPort, waitForValue } from './harness.mjs';
import { startFakeAnthropic } from './fixtures/fake_anthropic_fixture.mjs';
import { sqliteExec, sqliteRows } from './sqlite.mjs';

const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const PORT = Number(process.env.E2E_PORT ?? 38439);
const WORKSPACE = `catalog-corruption-${process.pid}`;
const AGENT = 'catalog-corruption-agent';
const MODEL = 'catalog-corruption-model';
const FAKE_KEY = 'sk-catalog-corruption-fake'; // awaken-allow: secret
const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

function start(directory) {
  return spawnProduction(directory, PORT, {
    workspace: WORKSPACE,
    controlSealKey: '00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff',
    // Catalog corruption/recovery is provider-neutral. The fixture selects the
    // portable Workdir provider; namespace isolation has its own required-host
    // suite and must not make this state-machine matrix host-dependent.
    fields: { sandbox_tier: 'local' },
  });
}

async function ready(child) {
  await waitForPort(PORT, 180_000, child);
}

async function stop(child, signal = 'SIGINT') {
  if (signal === 'SIGINT') return stopServer(child);
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

async function authorModel(upstream) {
  const provider = await json('POST', 'config/provider-connections', {
    idempotency_key: 'catalog-corruption-provider',
    workspace_id: WORKSPACE,
    provider_id: 'anthropic',
    display_name: 'Anthropic',
    dialect: 'anthropic_messages',
    base_url: `${upstream.url}/v1/`,
    timeout_secs: 30,
    secret: FAKE_KEY,
  });
  assert.equal(provider.status, 201, JSON.stringify(provider.body));
  const agent = await json('PUT', `config/agents/${AGENT}`, {
    name: AGENT, model: { id: MODEL }, system: 'catalog recovery', max_steps: 2,
  });
  assert.equal(agent.status, 200, JSON.stringify(agent.body));
  const publication = await json('POST', `config/agents/${AGENT}/publish`);
  assert.equal(publication.status, 200, JSON.stringify(publication.body));
}

async function driveSession(sessionId, text, expectedStatus) {
  const response = await json('POST', `sessions/${sessionId}/events`, {
    events: [{ type: 'user.message', content: [{ type: 'text', text }] }],
  });
  assert.equal(response.status, expectedStatus, JSON.stringify(response.body));
}

function seedRepository(root) {
  const work = path.join(root, 'catalog-repository-work');
  const remote = path.join(root, 'catalog-repository.git');
  fs.mkdirSync(work, { recursive: true });
  execFileSync('git', ['init', '-q'], { cwd: work });
  execFileSync('git', ['symbolic-ref', 'HEAD', 'refs/heads/main'], { cwd: work });
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
  const rows = sqliteRows(
    database,
    `SELECT data FROM resource_catalog_entry WHERE kind=${sqlQuote(kind)} AND id=${sqlQuote(id)}`,
  );
  assert.equal(rows.length, 1, `missing ${kind}/${id} catalog aggregate`);
  return JSON.parse(rows[0].data);
}

function writeCatalogRecord(database, kind, id, record) {
  writeCatalogRaw(database, kind, id, JSON.stringify(record));
}

function writeCatalogRaw(database, kind, id, data) {
  sqliteExec(
    database,
    `UPDATE resource_catalog_entry SET data=${sqlQuote(data)} WHERE kind=${
      sqlQuote(kind)
    } AND id=${sqlQuote(id)}`,
  );
}

function sessionAggregate(database, sessionId) {
  const rows = sqliteRows(
    database,
    `SELECT aggregate_json FROM managed_session WHERE session_id=${sqlQuote(sessionId)}`,
  );
  assert.equal(rows.length, 1, `missing Session ${sessionId}`);
  assert.ok(rows[0].aggregate_json, `Session ${sessionId} has no canonical aggregate`);
  return JSON.parse(rows[0].aggregate_json);
}

function sessionResources(database, sessionId) {
  return sessionAggregate(database, sessionId).resources;
}

function persistPreparedGeneration(database, sessionId) {
  const aggregate = sessionAggregate(database, sessionId);
  const state = aggregate.resources;
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
  sqliteExec(
    database,
    `UPDATE managed_session SET aggregate_json=${sqlQuote(JSON.stringify({
      ...aggregate,
      resources: next,
    }))} WHERE session_id=${sqlQuote(sessionId)}`,
  );
}

async function main() {
  const directory = fs.mkdtempSync(path.join(os.tmpdir(), 'awaken-catalog-corruption-'));
  const upstream = await startFakeAnthropic(FAKE_KEY, { models: [MODEL] });
  const resourceDatabase = path.join(directory, 'resources.db');
  const sessionsDatabase = path.join(directory, 'sessions.db');
  let server = start(directory);
  try {
    await ready(server);
    await authorModel(upstream);
    const memory = await json('POST', 'memory_stores', {
      name: 'catalog-fail-closed',
      description: 'configuration must never be inferred',
    });
    assert.equal(memory.status, 200, JSON.stringify(memory.body));

    const repository = seedRepository(directory);
    const session = await json('POST', 'sessions', {
      agent: AGENT,
      environment_id: 'env_local',
      resources: [{
        type: 'github_repository',
        url: repository,
        mount_path: '/workspace/catalog-repository',
      }],
    });
    assert.equal(session.status, 200, JSON.stringify(session.body));
    const repositoryId = `managed:${session.body.id}:repository:0`;

    // Demand/placement cause graph: a Session create freezes Resource intent
    // but a registered Worker owns physical realization. Only an actual Run
    // claim may establish the Active baseline used by the corruption experiment.
    //
    // | Rule | Worker eligible | Run demand | Catalog valid | Effect |
    // |---|---|---|---|---|
    // | B1 | yes | no | yes | remain Prepared |
    // | B2 | yes | yes | yes | Active baseline |
    await driveSession(session.body.id, 'establish catalog baseline', 200);
    await waitForValue(
      () => sessionResources(sessionsDatabase, session.body.id),
      (resources) => resources.pending === undefined
        && resources.activations.some((activation) => activation.state === 'active'),
      'initial catalog Resource generation did not become Active',
    );

    await stop(server, 'SIGKILL');
    const memoryRecord = catalogRecord(resourceDatabase, 'memory_store', memory.body.id);
    const repositoryRecord = catalogRecord(resourceDatabase, 'repository', repositoryId);
    writeCatalogRecord(resourceDatabase, 'memory_store', memory.body.id, {
      ...memoryRecord,
      configs: {},
    });
    writeCatalogRecord(resourceDatabase, 'repository', repositoryId, {
      ...repositoryRecord,
      configs: {},
    });
    persistPreparedGeneration(sessionsDatabase, session.body.id);

    server = start(directory);
    await ready(server);

    // Corruption decision table: C1 exact pending generation; C2 immutable
    // catalog history exists; C3 real Run demand. C1+!C2+C3 is rejected by the
    // Coordinator binding verifier before enqueue, so attempts stays zero and
    // no fabricated Worker error is recorded. Restoring C2 and redelivering C3
    // performs the first real attempt and commits that same generation. Listener
    // readiness alone performs no hidden Worker-owned effect.
    await driveSession(session.body.id, 'observe corrupt catalog generation', 400);

    // Cause/effect boundary rule: missing internal catalog config can fail
    // resource lifecycle/binding, but cannot make removed HTTP routes reappear.
    assert.equal((await json('GET', `memory_stores/${memory.body.id}/config`)).status, 404);
    assert.equal(
      (await json('GET', `memory_stores/${memory.body.id}/config_versions/1`)).status,
      404,
    );
    assert.equal(
      (await json('POST', `memory_stores/${memory.body.id}/config`, {
        expected_config_version: 1,
        recall_policy: { enabled: true },
      })).status,
      404,
    );
    assert.equal((await json('DELETE', `memory_stores/${memory.body.id}`)).status, 500);
    const deniedMemoryBinding = await json('POST', 'sessions', {
      agent: 'assistant',
      environment_id: 'env_local',
      resources: [{
        type: 'memory_store',
        memory_store_id: memory.body.id,
        mount_path: '/workspace/memory',
      }],
    });
    assert.equal(deniedMemoryBinding.status, 400);
    assert.match(JSON.stringify(deniedMemoryBinding.body), /current config version is missing/u);

    const deniedRepository = await waitForValue(
      () => sessionResources(sessionsDatabase, session.body.id),
      (resources) => resources.pending !== undefined
        && resources.activations.at(-1).state === 'prepared'
        && resources.activations.at(-1).attempts === 0
        && !resources.activations.at(-1).last_error,
      'missing catalog config changed the unattempted pending generation',
    );
    assert.notEqual(deniedRepository.pending, undefined);
    assert.equal(deniedRepository.activations.at(-1).state, 'prepared');
    assert.equal(deniedRepository.activations.at(-1).attempts, 0);
    assert.equal(deniedRepository.activations.at(-1).last_error, undefined);

    // Repair only the missing immutable histories. The already-persisted Session
    // generation remains unchanged and must be the generation that later commits.
    await stop(server, 'SIGKILL');
    writeCatalogRecord(resourceDatabase, 'memory_store', memory.body.id, memoryRecord);
    writeCatalogRecord(resourceDatabase, 'repository', repositoryId, repositoryRecord);
    server = start(directory);
    await ready(server);
    await driveSession(session.body.id, 'retry repaired catalog generation', 200);

    const recovered = await waitForValue(
      () => sessionResources(sessionsDatabase, session.body.id),
      (resources) => resources.pending === undefined
        && resources.activations.at(-1).state === 'active',
      'repaired catalog generation did not commit',
    );
    assert.equal(recovered.pending, undefined);
    assert.equal(recovered.activations.at(-1).state, 'active');
    assert.equal(recovered.activations.at(-1).attempts, 1);
    assert.equal(recovered.activations.at(-1).last_error, undefined);
    assert.equal((await json('GET', `memory_stores/${memory.body.id}/config`)).status, 404);

    // Every catalog read validates the complete aggregate. Corrupt durable JSON
    // must fail closed on a cold process without panicking or serving a partial
    // definition/config history. This drives the production collection API so
    // the storage failure remains distinguishable from an ordinary 404.
    await stop(server);
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
      writeCatalogRaw(resourceDatabase, 'memory_store', memory.body.id, data);
      server = start(directory);
      await ready(server);
      const denied = await json('GET', 'memory_stores');
      assert.equal(denied.status, 500, `${name}: ${JSON.stringify(denied.body)}`);
      assert.match(JSON.stringify(denied.body), /resource catalog storage failure/u);
      assert.equal(server.exitCode, null, `${name}: catalog corruption crashed the process`);
      await stop(server);
    }
    writeCatalogRecord(resourceDatabase, 'memory_store', memory.body.id, memoryRecord);
    server = start(directory);
    await ready(server);
    assert.equal((await json('GET', 'memory_stores')).status, 200);

    // Repository aggregates have no standalone public collection route: they are
    // execution inputs owned by the Session lifecycle. Drive the same corruption
    // matrix through cold-start activation reconciliation. Every corrupt aggregate
    // must leave the already-frozen generation prepared (never silently re-resolve
    // from the remote), and restoring only the aggregate must converge that exact
    // generation on the next boot.
    const repositoryConfig = repositoryRecord.configs['1'];
    const repositoryCorruptions = [
      ['malformed-json', '{not-json'],
      ['forged-definition-id', JSON.stringify({
        ...repositoryRecord,
        definition: { ...repositoryRecord.definition, id: 'forged-repository-id' },
      })],
      ['empty-workspace', JSON.stringify({
        ...repositoryRecord,
        definition: { ...repositoryRecord.definition, workspace_id: ' ' },
      })],
      ['zero-current-version', JSON.stringify({
        ...repositoryRecord,
        definition: { ...repositoryRecord.definition, current_config_version: 0 },
      })],
      ['missing-current-version', JSON.stringify({ ...repositoryRecord, configs: {} })],
      ['forged-config-id', JSON.stringify({
        ...repositoryRecord,
        configs: { 1: { ...repositoryConfig, repository_id: 'forged-repository-id' } },
      })],
      ['forged-config-version', JSON.stringify({
        ...repositoryRecord,
        configs: { 1: { ...repositoryConfig, version: 2 } },
      })],
    ];
    for (const [name, data] of repositoryCorruptions) {
      await stop(server);
      writeCatalogRaw(resourceDatabase, 'repository', repositoryId, data);
      persistPreparedGeneration(sessionsDatabase, session.body.id);
      server = start(directory);
      await ready(server);
      await driveSession(session.body.id, `reject ${name} Repository catalog`, 400);

      const denied = await waitForValue(
        () => sessionResources(sessionsDatabase, session.body.id),
        (resources) => resources.pending !== undefined
          && resources.activations.at(-1).state === 'prepared'
          && resources.activations.at(-1).attempts === 0
          && !resources.activations.at(-1).last_error,
        `${name}: corruption changed an unattempted generation`,
      );
      assert.notEqual(denied.pending, undefined, `${name}: pending generation disappeared`);
      assert.equal(denied.activations.at(-1).state, 'prepared', name);
      assert.equal(denied.activations.at(-1).attempts, 0, name);
      assert.equal(denied.activations.at(-1).last_error, undefined, name);
      assert.equal(server.exitCode, null, `${name}: catalog corruption crashed the process`);

      await stop(server);
      writeCatalogRecord(resourceDatabase, 'repository', repositoryId, repositoryRecord);
      server = start(directory);
      await ready(server);
      await driveSession(session.body.id, `recover ${name} Repository catalog`, 200);
      // Cause/effect decision table for every corruption rule:
      // corrupt catalog + Run demand => admission rejects before an attempt;
      // restored catalog + Run demand => that same generation becomes Active;
      // listener readiness alone is never a Worker recovery receipt.
      const repaired = await waitForValue(
        () => sessionResources(sessionsDatabase, session.body.id),
        (resources) => resources.pending === undefined
          && resources.activations.at(-1).state === 'active',
        `${name}: repaired generation did not commit`,
      );
      assert.equal(repaired.pending, undefined, `${name}: repaired generation did not commit`);
      assert.equal(repaired.activations.at(-1).state, 'active', name);
      assert.equal(repaired.activations.at(-1).attempts, 1, name);
      assert.equal(repaired.activations.at(-1).last_error, undefined, name);
    }

    console.log('E2E PASS: corrupt Memory and Repository aggregates fail closed and the same snapshot later recovers.');
  } finally {
    await stop(server).catch(() => {});
    await upstream.close();
    fs.rmSync(directory, { recursive: true, force: true });
  }
}

main().catch((error) => {
  console.error('E2E FAIL:', error);
  process.exitCode = 1;
});
