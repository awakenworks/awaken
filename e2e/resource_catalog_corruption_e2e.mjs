// Fail-closed E2E for corrupt Resource Catalog aggregates. A definition whose
// current immutable config is missing must never fall back to defaults/current
// remote state. Restoring the same catalog data lets the persisted activation
// converge without changing its Session snapshot.

import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import {
  managedWorkspaceClient,
  spawnProduction,
  stopServer,
  waitForPort,
  waitForSessionEventReceipt,
  waitForValue,
} from './harness.mjs';
import { startFakeAnthropic } from './fixtures/fake_anthropic_fixture.mjs';
import { sqliteExec, sqliteRows } from './sqlite.mjs';

const PORT = Number(process.env.E2E_PORT ?? 38439);
const WORKSPACE = `catalog-corruption-${process.pid}`;
const AGENT = 'catalog-corruption-agent';
const MODEL = 'catalog-corruption-model';
const FAKE_KEY = 'sk-catalog-corruption-fake'; // awaken-allow: secret
const MANAGED_BETA = 'managed-agents-2026-04-01';
const MEMORY_BETA = 'agent-memory-2026-07-22';
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

async function internalJson(method, tail, body, beta) {
  // Agent publication and deliberately absent Awaken-only config routes have no
  // SDK method. Session and Memory compatibility calls below never enter here.
  const response = await fetch(scoped(tail), {
    method,
    headers: {
      ...(beta === undefined ? {} : { 'anthropic-beta': beta }),
      ...(body === undefined ? {} : { 'content-type': 'application/json' }),
    },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  const text = await response.text();
  return { status: response.status, body: text ? JSON.parse(text) : null };
}

async function authorModel(upstream) {
  const provider = await internalJson('POST', 'config/provider-connections', {
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
  const agent = await internalJson('PUT', `config/agents/${AGENT}`, {
    name: AGENT, model: { id: MODEL }, system: 'catalog recovery', max_steps: 2,
  });
  assert.equal(agent.status, 200, JSON.stringify(agent.body));
  const publication = await internalJson('POST', `config/agents/${AGENT}/publish`);
  assert.equal(publication.status, 200, JSON.stringify(publication.body));
}

async function driveSession(client, sessionId, text) {
  const response = await client.beta.sessions.events.send(sessionId, {
    events: [{ type: 'user.message', content: [{ type: 'text', text }] }],
    betas: [MANAGED_BETA],
  });
  // Durable-admission decision rules: D1 valid wire command => HTTP 200 with
  // one exact unprocessed receipt, regardless of whether Resource dependencies
  // are presently readable; D2 malformed wire command => synchronous 4xx.
  // Dependency failure belongs to later reconciliation and cannot roll D1 back.
  assert.equal(response.data?.length, 1, JSON.stringify(response));
  const receipt = response.data[0];
  assert.equal(receipt.type, 'user.message');
  assert.equal(receipt.processed_at, null);
  return receipt;
}

async function listMemoryStores(client) {
  const stores = [];
  for await (const store of client.beta.memoryStores.list({ betas: [MEMORY_BETA] })) {
    stores.push(store);
  }
  return stores;
}

function sdkErrorMatches(error, status, pattern) {
  return error?.status === status
    && pattern.test(`${String(error?.message)}\n${JSON.stringify(error?.error)}`);
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

function sessionEnvelope(database, sessionId) {
  const rows = sqliteRows(
    database,
    `SELECT aggregate_json FROM managed_session WHERE session_id=${sqlQuote(sessionId)}`,
  );
  assert.equal(rows.length, 1, `missing Session ${sessionId}`);
  assert.ok(rows[0].aggregate_json, `Session ${sessionId} has no canonical aggregate`);
  const envelope = JSON.parse(rows[0].aggregate_json);
  assert.equal(envelope.format, 'awaken.session.v1');
  return envelope;
}

function sessionAggregate(database, sessionId) {
  return sessionEnvelope(database, sessionId).aggregate;
}

function sessionResources(database, sessionId) {
  return sessionAggregate(database, sessionId).resources;
}

function persistPreparedGeneration(database, sessionId) {
  const envelope = sessionEnvelope(database, sessionId);
  const aggregate = envelope.aggregate;
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
      ...envelope,
      aggregate: {
        ...aggregate,
        resources: next,
      },
    }))} WHERE session_id=${sqlQuote(sessionId)}`,
  );
}

async function main() {
  // Test design (catalog corruption matrix). Causes: C1=a valid MemoryStore
  // aggregate establishes an Active generation; C2=durable catalog/
  // generation/manifest fields are missing, malformed, cross-linked, or stale;
  // C3=the process restarts and a new Run demands realization. Effects:
  // E1=the valid baseline executes; E2=every C2 arm fails closed with a
  // classified error and no model/resource success; E3=repair restores only the
  // original authoritative generation. Constraints/invariant: catalog rows,
  // immutable inputs, and matching manifest generation form one atomic truth.
  // Decision rules: C1=>E1; C1+C2+C3=>E2; repaired(C2)+C3=>E3.
  const directory = fs.mkdtempSync(path.join(os.tmpdir(), 'awaken-catalog-corruption-'));
  const upstream = await startFakeAnthropic(FAKE_KEY, { models: [MODEL] });
  const resourceDatabase = path.join(directory, 'resources.db');
  const sessionsDatabase = path.join(directory, 'sessions.db');
  let server = start(directory);
  try {
    await ready(server);
    await authorModel(upstream);
    const client = managedWorkspaceClient(`http://127.0.0.1:${PORT}`, WORKSPACE);
    const memory = await client.beta.memoryStores.create({
      name: 'catalog-fail-closed',
      description: 'configuration must never be inferred',
      betas: [MEMORY_BETA],
    });

    const session = await client.beta.sessions.create({
      agent: AGENT,
      environment_id: 'env_local',
      resources: [{
        type: 'memory_store',
        memory_store_id: memory.id,
        mount_path: '/workspace/catalog-memory',
      }],
      betas: [MANAGED_BETA],
    });

    // Demand/placement cause graph: a Session create freezes Resource intent
    // but a registered Worker owns physical realization. Only an actual Run
    // claim may establish the Active baseline used by the corruption experiment.
    //
    // | Rule | Worker eligible | Run demand | Catalog valid | Effect |
    // |---|---|---|---|---|
    // | B1 | yes | no | yes | remain Prepared |
    // | B2 | yes | yes | yes | exact receipt processed; Active baseline |
    const baselineReceipt = await driveSession(client, session.id, 'establish catalog baseline');
    await waitForValue(
      () => sessionResources(sessionsDatabase, session.id),
      (resources) => resources.pending === undefined
        && resources.activations.some((activation) => activation.state === 'active'),
      'initial catalog Resource generation did not become Active',
    );
    await waitForSessionEventReceipt(
      client,
      session.id,
      baselineReceipt.id,
      [MANAGED_BETA],
      () => true,
      'the catalog baseline command to process after its Resource generation becomes Active',
    );

    await stop(server, 'SIGKILL');
    const memoryRecord = catalogRecord(resourceDatabase, 'memory_store', memory.id);
    writeCatalogRecord(resourceDatabase, 'memory_store', memory.id, {
      ...memoryRecord,
      configs: {},
    });
    persistPreparedGeneration(sessionsDatabase, session.id);

    server = start(directory);
    await ready(server);

    // Corruption decision table: C1 exact pending generation; C2 immutable
    // catalog history exists; C3 real Run demand. C1+!C2+C3 commits the exact
    // Session-root receipt but reconciliation fails before enqueue, so attempts
    // stays zero and the receipt stays unprocessed. Restoring C2 lets that same
    // durable command perform the first real attempt and commit the generation;
    // no second User command is allowed. Listener readiness alone performs no
    // hidden Worker-owned effect.
    const deniedReceipt = await driveSession(
      client,
      session.id,
      'observe corrupt catalog generation',
    );

    // Cause/effect boundary rule: missing internal catalog config can fail
    // resource lifecycle/binding, but cannot make removed HTTP routes reappear.
    assert.equal(
      (await internalJson(
        'GET',
        `memory_stores/${memory.id}/config`,
        undefined,
        MEMORY_BETA,
      )).status,
      404,
    );
    assert.equal(
      (await internalJson(
        'GET',
        `memory_stores/${memory.id}/config_versions/1`,
        undefined,
        MEMORY_BETA,
      )).status,
      404,
    );
    assert.equal(
      (await internalJson('POST', `memory_stores/${memory.id}/config`, {
        expected_config_version: 1,
        recall_policy: { enabled: true },
      }, MEMORY_BETA)).status,
      404,
    );
    await assert.rejects(
      () => client.beta.memoryStores.delete(memory.id, { betas: [MEMORY_BETA] }),
      (error) => error?.status === 500,
    );
    await assert.rejects(
      () => client.beta.sessions.create({
        agent: 'assistant',
        environment_id: 'env_local',
        resources: [{
          type: 'memory_store',
          memory_store_id: memory.id,
          mount_path: '/workspace/memory',
        }],
        betas: [MANAGED_BETA],
      }),
      (error) => sdkErrorMatches(error, 400, /current config version is missing/u),
    );

    const deniedMemory = await waitForValue(
      () => sessionResources(sessionsDatabase, session.id),
      (resources) => resources.pending !== undefined
        && resources.activations.at(-1).state === 'prepared'
        && resources.activations.at(-1).attempts === 0
        && !resources.activations.at(-1).last_error,
      'missing catalog config changed the unattempted pending generation',
    );
    assert.notEqual(deniedMemory.pending, undefined);
    assert.equal(deniedMemory.activations.at(-1).state, 'prepared');
    assert.equal(deniedMemory.activations.at(-1).attempts, 0);
    assert.equal(deniedMemory.activations.at(-1).last_error, undefined);
    assert.equal(deniedReceipt.processed_at, null);

    // Repair only the missing immutable histories. The already-persisted Session
    // generation remains unchanged and must be the generation that later commits.
    await stop(server, 'SIGKILL');
    writeCatalogRecord(resourceDatabase, 'memory_store', memory.id, memoryRecord);
    server = start(directory);
    await ready(server);

    const recovered = await waitForValue(
      () => sessionResources(sessionsDatabase, session.id),
      (resources) => resources.pending === undefined
        && resources.activations.at(-1).state === 'active',
      'repaired catalog generation did not commit',
      // SIGKILL preserves the predecessor's canonical 60-second Session Work
      // lease. Before expiry a replacement must leave this exact generation
      // Prepared; after expiry it claims the already-retained User command and
      // commits it. Ninety seconds covers lease+claim latency without sending a
      // second command or accepting any weaker state.
      { timeoutMs: 90_000 },
    );
    assert.equal(recovered.pending, undefined);
    assert.equal(recovered.activations.at(-1).state, 'active');
    assert.equal(recovered.activations.at(-1).attempts, 1);
    assert.equal(recovered.activations.at(-1).last_error, undefined);
    await waitForSessionEventReceipt(
      client,
      session.id,
      deniedReceipt.id,
      [MANAGED_BETA],
      () => true,
      'the repaired catalog to process the original durable command',
    );
    assert.equal(
      (await internalJson(
        'GET',
        `memory_stores/${memory.id}/config`,
        undefined,
        MEMORY_BETA,
      )).status,
      404,
    );

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
      writeCatalogRaw(resourceDatabase, 'memory_store', memory.id, data);
      server = start(directory);
      await ready(server);
      await assert.rejects(
        () => listMemoryStores(client),
        (error) => sdkErrorMatches(error, 500, /resource registry data is corrupt/u),
        `${name}: corrupt Memory catalog must fail through the SDK`,
      );
      assert.equal(server.exitCode, null, `${name}: catalog corruption crashed the process`);
      await stop(server);
    }
    writeCatalogRecord(resourceDatabase, 'memory_store', memory.id, memoryRecord);
    server = start(directory);
    await ready(server);
    assert.ok((await listMemoryStores(client)).some((store) => store.id === memory.id));

    console.log('E2E PASS: corrupt MemoryStore aggregates fail closed and the same snapshot later recovers.');
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
