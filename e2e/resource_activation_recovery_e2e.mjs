// Cause-effect E2E for durable Session resource activation recovery. The test
// stops the production `awaken` process at the two persisted crash windows and
// proves startup reconciliation converges without an IAM/policy dependency.

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
const PORT = Number(process.env.E2E_PORT ?? 38436);
const WORKSPACE = `activation-recovery-${process.pid}`;
const AGENT = 'activation-recovery-agent';
const MODEL = 'activation-recovery-model';
const FAKE_KEY = 'sk-activation-recovery-fake'; // awaken-allow: secret
const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

function start(directory) {
  return spawnProduction(directory, PORT, {
    workspace: WORKSPACE,
    controlSealKey: '00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff',
    // This scenario validates the resource activation state machine, not the
    // host's bwrap/Seatbelt availability. Select the explicit local provider in
    // the fixture's typed deployment instead of inheriting production's
    // fail-closed namespace default.
    fields: { sandbox_tier: 'local' },
  });
}

async function ready() {
  await waitForPort(PORT, 60_000);
}

async function stop(child, signal = 'SIGINT') {
  if (signal === 'SIGINT') return stopServer(child);
  if (child.exitCode !== null) return;
  child.kill(signal);
  await new Promise((resolve) => child.once('exit', resolve));
}

const scoped = (suffix) =>
  `http://127.0.0.1:${PORT}/v1/workspaces/${WORKSPACE}/${suffix}`;

async function json(method, url, body) {
  const response = await fetch(url, {
    method,
    headers: body === undefined ? {} : { 'content-type': 'application/json' },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  const text = await response.text();
  return { status: response.status, body: text ? JSON.parse(text) : null };
}

async function authorModel(upstream) {
  const provider = await json('POST', scoped('config/provider-connections'), {
    idempotency_key: 'activation-recovery-provider',
    workspace_id: WORKSPACE,
    provider_id: 'anthropic',
    display_name: 'Anthropic',
    dialect: 'anthropic_messages',
    base_url: `${upstream.url}/v1/`,
    timeout_secs: 30,
    secret: FAKE_KEY,
  });
  assert.equal(provider.status, 201, JSON.stringify(provider.body));
  const agent = await json('PUT', scoped(`config/agents/${AGENT}`), {
    name: AGENT, model: { id: MODEL }, system: 'resource recovery', max_steps: 2,
  });
  assert.equal(agent.status, 200, JSON.stringify(agent.body));
  const publication = await json('POST', scoped(`config/agents/${AGENT}/publish`));
  assert.equal(publication.status, 200, JSON.stringify(publication.body));
}

async function driveSession(sessionId, text) {
  const response = await json('POST', scoped(`sessions/${sessionId}/events`), {
    events: [{ type: 'user.message', content: [{ type: 'text', text }] }],
  });
  // Demand-driver decision rule: a published model + valid Run demand -> 200
  // only after the Worker realizes the exact Resource generation and inference
  // commits. Every error remains visible instead of being treated as recovery.
  assert.equal(response.status, 200, JSON.stringify(response.body));
}

function seedRepository(root) {
  const work = path.join(root, 'activation-repository-work');
  const remote = path.join(root, 'activation-repository.git');
  fs.mkdirSync(work, { recursive: true });
  execFileSync('git', ['init', '-q'], { cwd: work });
  execFileSync('git', ['symbolic-ref', 'HEAD', 'refs/heads/main'], { cwd: work });
  execFileSync('git', ['config', 'user.email', 'activation@example.invalid'], { cwd: work });
  execFileSync('git', ['config', 'user.name', 'activation-recovery'], { cwd: work });
  fs.writeFileSync(path.join(work, 'README.md'), 'activation recovery');
  execFileSync('git', ['add', 'README.md'], { cwd: work });
  execFileSync('git', ['commit', '-q', '-m', 'seed'], { cwd: work });
  execFileSync('git', ['clone', '-q', '--bare', work, remote]);
  return remote;
}

function sqlQuote(value) {
  return `'${String(value).replaceAll("'", "''")}'`;
}

function sqlite(database, sql) {
  return sqliteExec(database, sql);
}

function sessionRow(database, sessionId) {
  const rows = sqliteRows(
    database,
    `SELECT aggregate_json FROM managed_session WHERE session_id=${sqlQuote(sessionId)}`,
  );
  assert.equal(rows.length, 1, `missing durable Session ${sessionId}`);
  assert.ok(rows[0].aggregate_json, `Session ${sessionId} has no canonical aggregate`);
  const aggregate = JSON.parse(rows[0].aggregate_json);
  return {
    aggregate,
    status: aggregate.status,
    archivedAt: aggregate.archived_at,
    resources: aggregate.resources,
  };
}

function updateSessionRow(database, sessionId, status, archivedAt, resources) {
  const row = sessionRow(database, sessionId);
  const aggregate = {
    ...row.aggregate,
    status,
    archived_at: archivedAt,
    resources,
  };
  sqliteExec(
    database,
    `UPDATE managed_session SET aggregate_json=${sqlQuote(JSON.stringify(aggregate))}
       WHERE session_id=${sqlQuote(sessionId)}`,
  );
}

function persistPreparedGeneration(database, sessionId) {
  const row = sessionRow(database, sessionId);
  assert.equal(row.status, 'idle');
  assert.equal(row.resources.pending, undefined);
  const revision = row.resources.revision + 1;
  const previous = row.resources.activations.map((activation) => ({
    ...activation,
    state: activation.state === 'active' ? 'releasing' : activation.state,
  }));
  const prepared = row.resources.activations
    .filter((activation) => activation.state === 'active')
    .map((activation) => ({
      ...activation,
      activation_id: `${sessionId}:${revision}:${activation.binding_id}`,
      revision,
      state: 'prepared',
      attempts: 0,
      last_error: 'process died before realization',
    }));
  assert.ok(prepared.length > 0);
  updateSessionRow(database, sessionId, 'idle', null, {
    revision,
    active: row.resources.active,
    pending: row.resources.active,
    activations: [...previous, ...prepared],
  });
}

function persistTerminalRelease(database, sessionId) {
  const row = sessionRow(database, sessionId);
  assert.equal(row.status, 'idle');
  const resources = {
    ...row.resources,
    activations: row.resources.activations.map((activation) => ({
      ...activation,
      state: activation.state === 'active' ? 'releasing' : activation.state,
    })),
  };
  assert.ok(resources.activations.some((activation) => activation.state === 'releasing'));
  updateSessionRow(database, sessionId, 'terminated', '2026-07-22T00:00:00Z', resources);
}

function persistLegacyManifest(database, sessionId) {
  const row = sessionRow(database, sessionId);
  assert.equal(row.status, 'idle');
  assert.equal(row.resources.pending, undefined);
  assert.ok(row.resources.active.inputs.length > 0);
  // Exercise the retained one-way row decoder deliberately: a genuine legacy
  // row owns Agent, model, Environment, and resource facts in retained columns,
  // never inside aggregate_json. A canonical-era row deliberately leaves those
  // columns blank, so nulling only aggregate_json would fabricate storage
  // corruption rather than a historical row. Preserve the complete legacy
  // causes here so recovery tests the supported migration contract.
  //
  // Legacy-row decision rule: aggregate absent + retained identity complete =>
  // decode and adopt once; aggregate absent + identity absent is corruption and
  // must not be described as a successful legacy recovery.
  sqliteExec(
    database,
    `UPDATE managed_session
       SET aggregate_json=NULL, agent_id=${sqlQuote(AGENT)}, model=${sqlQuote(MODEL)},
           environment_id='env_local', status='idle', archived_at=NULL,
           effective_inputs_json=${sqlQuote(JSON.stringify(row.resources.active))}
       WHERE session_id=${sqlQuote(sessionId)}`,
  );
}

function persistInconsistentRelease(database, sessionId) {
  const row = sessionRow(database, sessionId);
  const resources = {
    ...row.resources,
    pending: undefined,
    activations: row.resources.activations.map((activation) => ({
      ...activation,
      state: activation.state === 'active' ? 'releasing' : activation.state,
    })),
  };
  assert.ok(resources.activations.some((activation) => activation.state === 'releasing'));
  updateSessionRow(database, sessionId, 'idle', null, resources);
}

function repositoryRecord(database, id) {
  const rows = sqliteRows(
    database,
    `SELECT data FROM resource_catalog_entry WHERE kind='repository' AND id=${sqlQuote(id)}`,
  );
  assert.equal(rows.length, 1, `missing Repository aggregate ${id}`);
  return rows[0].data;
}

function receipts(directory) {
  const database = path.join(directory, 'resources.db');
  if (!fs.existsSync(database)) return [];
  return sqliteRows(database, 'SELECT data FROM resource_lifecycle_purge_intents')
    .map((row) => JSON.parse(row.data));
}

async function waitRepositoryReceipt(directory, resourceId) {
  const deadline = Date.now() + 20_000;
  while (Date.now() < deadline) {
    const receipt = receipts(directory).find((intent) =>
      intent.target.kind === 'repository'
      && intent.target.resource_id === resourceId
      && intent.status === 'completed');
    if (receipt) return receipt;
    await sleep(200);
  }
  throw new Error(`no completed Repository receipt for ${resourceId}`);
}

async function main() {
  const directory = fs.mkdtempSync(path.join(os.tmpdir(), 'awaken-activation-recovery-'));
  const upstream = await startFakeAnthropic(FAKE_KEY, { models: [MODEL] });
  const sessionsDatabase = path.join(directory, 'sessions.db');
  // Resource Catalog and lifecycle are independently migrated aggregates owned
  // by the one Resources component and persisted in its one database.
  const resourceDatabase = path.join(directory, 'resources.db');
  let server = start(directory);
  try {
    await ready();
    await authorModel(upstream);

    const malformed = await json('POST', scoped('sessions'), {
      agent: AGENT,
      resources: [{ type: 'unsupported-resource' }],
    });
    assert.equal(malformed.status, 400);

    // A Workdir-backed MemoryStore exercises the same activation state machine
    // without claiming the local sandbox can enforce a read-only File mount.
    // File isolation belongs to the namespace/container suites.
    const memoryStore = await json('POST', scoped('memory_stores'), {
      name: 'activation-recovery-memory',
    });
    assert.equal(memoryStore.status, 200, JSON.stringify(memoryStore.body));
    const memoryStoreId = memoryStore.body.id;
    const recovering = await json('POST', scoped('sessions'), {
      agent: AGENT,
      environment_id: 'env_local',
      resources: [{
        type: 'memory_store', memory_store_id: memoryStoreId, mount_path: '/workspace/recovery',
      }],
    });
    assert.equal(recovering.status, 200, JSON.stringify(recovering.body));

    const legacy = await json('POST', scoped('sessions'), {
      agent: AGENT,
      environment_id: 'env_local',
      resources: [{
        type: 'memory_store', memory_store_id: memoryStoreId, mount_path: '/workspace/legacy',
      }],
    });
    assert.equal(legacy.status, 200, JSON.stringify(legacy.body));

    const repository = seedRepository(directory);
    const terminating = await json('POST', scoped('sessions'), {
      agent: AGENT,
      environment_id: 'env_local',
      resources: [{
        type: 'github_repository',
        url: repository,
        mount_path: '/workspace/terminal-repository',
      }],
    });
    assert.equal(terminating.status, 200, JSON.stringify(terminating.body));

    const inconsistent = await json('POST', scoped('sessions'), {
      agent: AGENT,
      environment_id: 'env_local',
      resources: [{
        type: 'memory_store', memory_store_id: memoryStoreId, mount_path: '/workspace/inconsistent',
      }],
    });
    assert.equal(inconsistent.status, 200, JSON.stringify(inconsistent.body));

    const cleanupCases = [];
    for (const name of ['catalog-read', 'purge-schedule', 'catalog-write', 'already-gone']) {
      const created = await json('POST', scoped('sessions'), {
        agent: AGENT,
        environment_id: 'env_local',
        resources: [{
          type: 'github_repository',
          url: repository,
          mount_path: `/workspace/${name}`,
        }],
      });
      assert.equal(created.status, 200, `${name}: ${JSON.stringify(created.body)}`);
      cleanupCases.push({
        name,
        sessionId: created.body.id,
        repositoryId: `managed:${created.body.id}:repository:0`,
      });
    }

    // Initial-realization cause/effect table: C1 listener ready; C2 a real Run
    // is claimed by the registered Worker; C3 every initial Resource generation
    // is Active. C1 alone legitimately leaves a demand-driven remote projection
    // `preparing`; only C1+C2+C3 is a valid baseline for injecting a later
    // replacement/release crash window.
    //
    // | Rule | C1 | C2 | C3 | Effect |
    // |---|---|---|---|---|
    // | B1 | yes | no | no | retain Prepared; no Coordinator-side effect |
    // | B2 | yes | yes | yes | stable idle baseline; crash injection is valid |
    const baselineSessionIds = [
      recovering.body.id,
      legacy.body.id,
      terminating.body.id,
      inconsistent.body.id,
      ...cleanupCases.map((cleanup) => cleanup.sessionId),
    ];
    for (const sessionId of baselineSessionIds) {
      await driveSession(sessionId, `establish active baseline for ${sessionId}`);
    }
    await waitForValue(
      () => baselineSessionIds.map((sessionId) => sessionRow(sessionsDatabase, sessionId)),
      (rows) => rows.every((row) => row.status === 'idle'
        && row.resources.pending === undefined
        && row.resources.activations.every((activation) => activation.state === 'active')),
      'initial Resource generations did not converge before crash injection',
      { timeoutMs: 45_000 },
    );

    // Model process death after each first durable edge: Prepared for a live
    // replacement, and Releasing for a terminal Session. These are precisely
    // the states the coordinator persists before external sandbox/catalog IO.
    await stop(server, 'SIGKILL');
    persistPreparedGeneration(sessionsDatabase, recovering.body.id);
    persistLegacyManifest(sessionsDatabase, legacy.body.id);
    persistTerminalRelease(sessionsDatabase, terminating.body.id);
    persistInconsistentRelease(sessionsDatabase, inconsistent.body.id);
    for (const cleanup of cleanupCases) {
      persistTerminalRelease(sessionsDatabase, cleanup.sessionId);
    }

    const catalogRead = cleanupCases.find((entry) => entry.name === 'catalog-read');
    const purgeSchedule = cleanupCases.find((entry) => entry.name === 'purge-schedule');
    const catalogWrite = cleanupCases.find((entry) => entry.name === 'catalog-write');
    const alreadyGone = cleanupCases.find((entry) => entry.name === 'already-gone');
    const catalogReadRecord = repositoryRecord(resourceDatabase, catalogRead.repositoryId);
    sqlite(
      resourceDatabase,
      `
        UPDATE resource_catalog_entry SET data='{broken-repository-aggregate'
          WHERE kind='repository' AND id=${sqlQuote(catalogRead.repositoryId)};
        CREATE TRIGGER reject_repository_state_update
          BEFORE UPDATE ON resource_catalog_entry
          WHEN OLD.kind='repository' AND OLD.id=${sqlQuote(catalogWrite.repositoryId)}
        BEGIN
          SELECT RAISE(ABORT, 'injected Repository lifecycle write failure');
        END;
        DELETE FROM resource_catalog_entry
          WHERE kind='repository' AND id=${sqlQuote(alreadyGone.repositoryId)};
      `,
    );
    sqlite(
      resourceDatabase,
      `
        CREATE TRIGGER reject_repository_purge_schedule
          BEFORE INSERT ON resource_lifecycle_purge_intents
          WHEN NEW.data LIKE ${sqlQuote(`%${purgeSchedule.repositoryId}%`)}
        BEGIN
          SELECT RAISE(ABORT, 'injected Repository purge scheduling failure');
        END;
      `,
    );

    server = start(directory);
    await ready();

    // Recovery cause/effect graph: listener readiness starts the authoritative
    // Coordinator supervisor but cannot perform Worker-owned physical effects.
    // A Prepared remote generation (C1) therefore remains pending until a real
    // Run demand is claimed (C2), then the Worker realizes that exact generation
    // (E1). A terminal Releasing generation (C3) is Coordinator cleanup work and
    // converges in the background (E2).
    //
    // Decision table:
    // | Rule | durable state | HTTP ready | Run demand | required observation |
    // | R1   | Prepared      | yes        | no | remains Prepared, no attempt |
    // | R2   | Prepared      | yes        | yes | Active + no pending |
    // | R3   | Releasing     | yes        | n/a | Released in background |
    // | R4   | faulted edge  | yes        | n/a | remains Releasing until repaired |
    //
    await sleep(500);
    const demandPending = sessionRow(sessionsDatabase, recovering.body.id);
    assert.ok(demandPending.resources.pending, 'R1 retains the durable generation');
    assert.equal(demandPending.resources.activations.at(-1).attempts, 0, 'R1 has no hidden effect');
    await driveSession(recovering.body.id, 'recover prepared Resource generation');
    await driveSession(legacy.body.id, 'adopt legacy Resource generation');

    // The bounded durable-state waits cover R2/R3 without creating a second
    // readiness contract or racing the sole supervisor/Worker claim path.
    const recovered = await waitForValue(
      () => sessionRow(sessionsDatabase, recovering.body.id),
      (row) => row.resources.pending === undefined
        && row.resources.activations.map((activation) => activation.state).join(',')
          === 'released,active',
      'prepared resource generation did not recover',
    );
    assert.equal(recovered.status, 'idle');
    assert.equal(recovered.resources.pending, undefined);
    assert.deepEqual(
      recovered.resources.activations.map((activation) => activation.state),
      ['released', 'active'],
    );
    assert.equal(recovered.resources.activations[1].attempts, 1);
    assert.equal(recovered.resources.activations[1].last_error, undefined);

    // Legacy resource state stored only the resolved manifest. The durable
    // live-inbox ingress reads the same Session aggregate after restart,
    // realizes that manifest, and upgrades it to the activation state machine.
    const legacyLookup = await json(
      'GET',
      scoped(`awaken/sessions/${legacy.body.id}/live-inbox`),
    );
    assert.equal(legacyLookup.status, 200);
    const upgraded = sessionRow(sessionsDatabase, legacy.body.id);
    assert.equal(upgraded.status, 'idle');
    assert.equal(upgraded.resources.revision, 1);
    assert.equal(upgraded.resources.pending, undefined);
    assert.equal(upgraded.resources.activations.length, 1);
    assert.equal(upgraded.resources.activations[0].state, 'active');
    assert.equal(upgraded.resources.activations[0].attempts, 1);
    assert.equal(upgraded.resources.activations[0].last_error, undefined);

    const released = await waitForValue(
      () => sessionRow(sessionsDatabase, terminating.body.id),
      (row) => row.resources.pending === undefined
        && row.resources.activations.every((activation) => activation.state === 'released'),
      'terminal resource generation did not release',
    );
    assert.equal(released.status, 'terminated');
    assert.equal(released.resources.pending, undefined);
    assert.ok(released.resources.activations.every((activation) => activation.state === 'released'));

    const repositoryId = `managed:${terminating.body.id}:repository:0`;
    const receipt = await waitRepositoryReceipt(directory, repositoryId);
    assert.equal(receipt.receipt.evidence.local_realizations_deleted, 0);

    // Each terminal cleanup error is durable and fail-closed. A missing catalog
    // row is the idempotent "already physically gone" case and can complete;
    // malformed catalog state and failed writes/scheduling must remain Releasing.
    const inconsistentState = sessionRow(sessionsDatabase, inconsistent.body.id);
    assert.equal(inconsistentState.status, 'idle');
    assert.ok(inconsistentState.resources.activations.some(
      (activation) => activation.state === 'releasing',
    ));
    for (const cleanup of cleanupCases.filter((entry) => entry.name !== 'already-gone')) {
      const pending = sessionRow(sessionsDatabase, cleanup.sessionId);
      assert.equal(pending.status, 'terminated', cleanup.name);
      assert.ok(
        pending.resources.activations.some((activation) => activation.state === 'releasing'),
        cleanup.name,
      );
    }
    await waitForValue(
      () => sessionRow(sessionsDatabase, alreadyGone.sessionId),
      (row) => row.resources.activations.every(
        (activation) => activation.state === 'released'),
      'already-absent Repository did not converge to Released',
    );

    // Repair only the failed durable dependencies. The next process must finish
    // every original cleanup intent without another API transition.
    await stop(server, 'SIGKILL');
    const repairedInconsistent = sessionRow(sessionsDatabase, inconsistent.body.id);
    repairedInconsistent.resources.activations = repairedInconsistent.resources.activations.map(
      (activation) => ({
        ...activation,
        state: activation.state === 'releasing' ? 'active' : activation.state,
      }),
    );
    updateSessionRow(
      sessionsDatabase,
      inconsistent.body.id,
      repairedInconsistent.status,
      repairedInconsistent.archivedAt,
      repairedInconsistent.resources,
    );
    sqlite(
      resourceDatabase,
      `
        UPDATE resource_catalog_entry SET data=${sqlQuote(catalogReadRecord)}
          WHERE kind='repository' AND id=${sqlQuote(catalogRead.repositoryId)};
        DROP TRIGGER reject_repository_state_update;
      `,
    );
    sqlite(resourceDatabase, 'DROP TRIGGER reject_repository_purge_schedule;');
    server = start(directory);
    await ready();
    for (const cleanup of cleanupCases.filter((entry) => entry.name !== 'already-gone')) {
      const settled = await waitForValue(
        () => sessionRow(sessionsDatabase, cleanup.sessionId),
        (row) => row.resources.activations.every(
          (activation) => activation.state === 'released'),
        `${cleanup.name} cleanup did not settle after dependency repair`,
      );
      assert.equal(settled.status, 'terminated', cleanup.name);
      assert.equal(
        (await waitRepositoryReceipt(directory, cleanup.repositoryId))
          .receipt.evidence.local_realizations_deleted,
        0,
      );
    }

    console.log('E2E PASS: prepared, legacy, inconsistent, and faulted terminal resource states recover after process death.');
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
