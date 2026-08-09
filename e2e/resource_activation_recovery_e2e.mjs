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
import { sqliteExec, sqliteRows } from './sqlite.mjs';

const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const PORT = Number(process.env.E2E_PORT ?? 38436);
const WORKSPACE = `activation-recovery-${process.pid}`;
const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

function start(directory) {
  return spawnProduction(directory, PORT, {
    workspace: WORKSPACE,
    controlSealKey: '00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff',
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

async function upload(content, filename) {
  const form = new FormData();
  form.append('purpose', 'agent');
  form.append('file', new Blob([content]), filename);
  const response = await fetch(scoped('files'), { method: 'POST', body: form });
  assert.equal(response.status, 200);
  return (await response.json()).id;
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
  // Exercise the retained one-way row decoder deliberately: legacy resource
  // manifests exist only in retained columns, never inside aggregate_json.
  // Use the shared in-process SQLite helper so this crash-window mutation has
  // identical behavior on developer machines without an external sqlite3 CLI.
  sqliteExec(
    database,
    `UPDATE managed_session
       SET aggregate_json=NULL, status='idle', archived_at=NULL,
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
  const sessionsDatabase = path.join(directory, 'sessions.db');
  // Resource Catalog and lifecycle are independently migrated aggregates owned
  // by the one Resources component and persisted in its one database.
  const resourceDatabase = path.join(directory, 'resources.db');
  let server = start(directory);
  try {
    await ready();

    const malformed = await json('POST', scoped('sessions'), {
      agent: 'assistant',
      resources: [{ type: 'unsupported-resource' }],
    });
    assert.equal(malformed.status, 400);

    const fileId = await upload('recover this binding', 'recovery.txt');
    const recovering = await json('POST', scoped('sessions'), {
      agent: 'assistant',
      environment_id: 'env_local',
      resources: [{ type: 'file', file_id: fileId, mount_path: '/workspace/recovery.txt' }],
    });
    assert.equal(recovering.status, 200, JSON.stringify(recovering.body));

    const legacy = await json('POST', scoped('sessions'), {
      agent: 'assistant',
      environment_id: 'env_local',
      resources: [{ type: 'file', file_id: fileId, mount_path: '/workspace/legacy.txt' }],
    });
    assert.equal(legacy.status, 200, JSON.stringify(legacy.body));

    const repository = seedRepository(directory);
    const terminating = await json('POST', scoped('sessions'), {
      agent: 'assistant',
      environment_id: 'env_local',
      resources: [{
        type: 'github_repository',
        url: repository,
        mount_path: '/workspace/terminal-repository',
      }],
    });
    assert.equal(terminating.status, 200, JSON.stringify(terminating.body));

    const inconsistent = await json('POST', scoped('sessions'), {
      agent: 'assistant',
      environment_id: 'env_local',
      resources: [{ type: 'file', file_id: fileId, mount_path: '/workspace/inconsistent.txt' }],
    });
    assert.equal(inconsistent.status, 200, JSON.stringify(inconsistent.body));

    const cleanupCases = [];
    for (const name of ['catalog-read', 'purge-schedule', 'catalog-write', 'already-gone']) {
      const created = await json('POST', scoped('sessions'), {
        agent: 'assistant',
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

    // Cause/effect graph: listener readiness starts one authoritative background
    // realization supervisor; it does not imply that durable recovery has
    // completed. A Prepared generation (C1) therefore remains pending until the
    // supervisor realizes it (E1), while a terminal Releasing generation (C2)
    // remains fenced until teardown completes (E2).
    //
    // Decision table:
    // | Rule | durable state | HTTP ready | required observation             |
    // | R1   | Prepared      | yes        | wait for Active + no pending     |
    // | R2   | Releasing     | yes        | wait for Released                |
    // | R3   | faulted edge  | yes        | remains Releasing until repaired |
    //
    // The bounded durable-state waits cover R1/R2 without creating a second
    // readiness contract or racing the sole supervisor.
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
      `http://127.0.0.1:${PORT}/v1/awaken/sessions/${legacy.body.id}/live-inbox`,
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
    fs.rmSync(directory, { recursive: true, force: true });
  }
}

main().catch((error) => {
  console.error('E2E FAIL:', error);
  process.exitCode = 1;
});
