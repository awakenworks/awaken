// Cause-effect E2E for ADR-0063 durable resource reclamation through the real
// `awaken` composition root. It covers process death after logical delete,
// shared-File retention, Memory head+history purge, Skill tombstone purge, and
// durable receipts. IAM is intentionally not queried by the background worker.

import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { execFileSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import { spawnProduction, stopServer, waitForPort } from './harness.mjs';
import { sqliteExec, sqliteRows, sqliteScalar } from './sqlite.mjs';

const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const PORT = Number(process.env.E2E_PORT ?? 38435);
const WS_A = `reclaim-a-${process.pid}`;
const WS_B = `reclaim-b-${process.pid}`;
const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

function start(directory) {
  return spawnProduction(directory, PORT, {
    workspace: WS_A,
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

const scoped = (workspace, suffix) =>
  `http://127.0.0.1:${PORT}/v1/workspaces/${workspace}/${suffix}`;

async function json(method, url, body) {
  const response = await fetch(url, {
    method,
    headers: body === undefined ? {} : { 'content-type': 'application/json' },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  const text = await response.text();
  return { status: response.status, body: text ? JSON.parse(text) : null };
}

async function upload(workspace, content, filename = 'input.txt') {
  const form = new FormData();
  form.append('purpose', 'agent');
  form.append('file', new Blob([content]), filename);
  const response = await fetch(scoped(workspace, 'files'), { method: 'POST', body: form });
  assert.equal(response.status, 200);
  return (await response.json()).id;
}

function receipts(directory) {
  const database = path.join(directory, 'resources.db');
  if (!fs.existsSync(database)) return [];
  return sqliteRows(database, 'SELECT data FROM resource_lifecycle_purge_intents')
    .map((row) => JSON.parse(row.data));
}

async function waitReceipt(
  directory,
  kind,
  resourceId,
  { workspace, timeoutMs = 45_000 } = {},
) {
  // Receipt identity decision table: R1 File logical id => match the canonical
  // delete idempotency key because the physical target is its content-addressed
  // blob; R2 every other resource id => match the target directly. Conflating
  // the two File identities would make deduplicated/shared blobs untraceable.
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    const receipt = receipts(directory).find(
      (intent) => intent.target.kind === kind
        && (kind === 'file'
          ? intent.idempotency_key === `file-delete:${workspace}:${resourceId}`
          : intent.target.resource_id === resourceId)
        && intent.status === 'completed',
    );
    if (receipt) return receipt;
    await sleep(200);
  }
  throw new Error(
    `no completed ${kind}/${resourceId} purge receipt: ${JSON.stringify(receipts(directory))}`,
  );
}

function scalar(database, sql) {
  return Number(sqliteScalar(database, sql));
}

function sqlQuote(value) {
  return `'${String(value).replaceAll("'", "''")}'`;
}

function blobForFile(directory, workspace, fileId) {
  // File id is the workspace-scoped logical handle; blob id is the canonical
  // physical row. Keep that mapping in one helper so assertions never query the
  // blob table with a logical id and pass accidentally on a missing row.
  return String(sqliteScalar(
    path.join(directory, 'files.db'),
    `SELECT blob_id FROM file_store_file
       WHERE id=${sqlQuote(fileId)} AND workspace_id=${sqlQuote(workspace)}`,
  ));
}

async function waitForLifecycleSchema(directory, timeoutMs = 20_000) {
  const database = path.join(directory, 'resources.db');
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    try {
      const version = scalar(
        database,
        `SELECT count(*) FROM resource_lifecycle_schema_migrations
           WHERE bundle_id='awaken.resource_lifecycle' AND version=1`,
      );
      if (version === 1) return version;
    } catch {
      // TCP readiness can precede optional resource-plane migration visibility.
      // A read-only open intentionally does not create a misleading empty DB.
    }
    await sleep(50);
  }
  throw new Error('resource lifecycle migration did not become visible');
}

function seedRepository(root) {
  const work = path.join(root, 'reclamation-repository-work');
  const remote = path.join(root, 'reclamation-repository.git');
  fs.mkdirSync(work, { recursive: true });
  execFileSync('git', ['init', '-q'], { cwd: work });
  execFileSync('git', ['symbolic-ref', 'HEAD', 'refs/heads/main'], { cwd: work });
  execFileSync('git', ['config', 'user.email', 'resource-reclaim@example.invalid'], { cwd: work });
  execFileSync('git', ['config', 'user.name', 'resource-reclaim'], { cwd: work });
  fs.writeFileSync(path.join(work, 'README.md'), 'reclamation repository');
  execFileSync('git', ['add', 'README.md'], { cwd: work });
  execFileSync('git', ['commit', '-q', '-m', 'seed'], { cwd: work });
  execFileSync('git', ['clone', '-q', '--bare', work, remote]);
  return remote;
}

async function main() {
  const directory = fs.mkdtempSync(path.join(os.tmpdir(), 'awaken-resource-reclaim-'));
  let server = start(directory);
  try {
    await ready();
    assert.equal(
      // Readiness cause graph: C1 TCP listener ready; C2 resource-plane DB exists;
      // C3 scoped migration is committed. Only C1+C2+C3 means the API composition
      // is fully observable; TCP alone must not let the fixture create an empty DB.
      await waitForLifecycleSchema(directory),
      1,
      'resource lifecycle schema is applied through its scoped migration ledger',
    );

    // Crash after the API committed durable intent + logical revoke. Startup
    // recovery must finish the same intent without another delete request.
    // Authentication decision table: R1 generic production fixture + no-login
    // identity => the scoped resource API is directly available; R2 an
    // explicitly self-managed fixture => bearer/session authentication is
    // required (covered by the management-auth E2Es). R1 must be configured in
    // the fixture's own config.toml, not inherited from another HOME.
    // The temporary intrinsic hold closes the scheduler race deterministically:
    // C1 durable delete + hold => intent remains pending before SIGKILL; C2 hold
    // removed while the process is dead => no second API command exists; C3
    // replacement generation becomes authoritative => startup recovery alone
    // completes the original intent.
    const crashedFile = await upload(WS_A, 'crash-recovery');
    const crashedBlob = blobForFile(directory, WS_A, crashedFile);
    sqliteExec(
      path.join(directory, 'resources.db'),
      `INSERT INTO resource_lifecycle_references(
         workspace_id, resource_kind, resource_id, reference_kind, reference_id
       ) VALUES (
         ${sqlQuote(WS_A)}, 'file', ${sqlQuote(crashedBlob)},
         'retention_hold', 'crash-recovery-hold'
       )`,
    );
    assert.equal((await json('DELETE', scoped(WS_A, `files/${crashedFile}`))).status, 200);
    await stop(server, 'SIGKILL');
    sqliteExec(
      path.join(directory, 'resources.db'),
      `DELETE FROM resource_lifecycle_references
         WHERE workspace_id=${sqlQuote(WS_A)}
           AND resource_kind='file'
           AND resource_id=${sqlQuote(crashedBlob)}
           AND reference_kind='retention_hold'
           AND reference_id='crash-recovery-hold'`,
    );
    server = start(directory);
    await ready();
    // Crash-recovery decision table: R1 graceful stop -> old generation is Dead
    // and replacement registers immediately; R2 SIGKILL + unexpired lease ->
    // replacement waits without stealing authority; R3 lease expiry -> the same
    // registration path advances the generation and resumes durable reclamation.
    const crashReceipt = await waitReceipt(directory, 'file', crashedFile, { workspace: WS_A });
    assert.equal(crashReceipt.receipt.evidence.blob_deleted, true);
    assert.equal(
      scalar(path.join(directory, 'files.db'),
        `SELECT count(*) FROM file_store_blob WHERE id=${sqlQuote(crashedBlob)}`),
      0,
    );

    // Authorization has already allowed the logical File deletion, but a live
    // Session binding is intrinsic resource state and independently blocks physical
    // reclamation. The reclaimer consumes only that reference edge; after Session
    // archive removes it, the same durable intent converges without an IAM query.
    const boundFile = await upload(WS_A, 'session-bound-content');
    const boundBlob = blobForFile(directory, WS_A, boundFile);
    const repository = seedRepository(directory);
    const boundSession = await json('POST', scoped(WS_A, 'sessions'), {
      agent: 'assistant', environment_id: 'env_local',
      resources: [{
        type: 'github_repository',
        url: repository,
        mount_path: '/workspace/reclamation-repository',
      }],
    });
    assert.equal(boundSession.status, 200, JSON.stringify(boundSession.body));
    const binding = await json(
      'POST',
      scoped(WS_A, `sessions/${boundSession.body.id}/resources`),
      { type: 'file', file_id: boundFile, mount_path: '/workspace/bound.txt' },
    );
    assert.equal(binding.status, 200, JSON.stringify(binding.body));
    assert.equal((await json('DELETE', scoped(WS_A, `files/${boundFile}`))).status, 200);
    await sleep(5_500);
    assert.ok(
      !receipts(directory).some(
        (intent) => intent.target.resource_id === boundFile && intent.status === 'completed',
      ),
      'live Session binding defers the physical purge',
    );
    assert.equal(
      scalar(
        path.join(directory, 'files.db'),
        `SELECT count(*) FROM file_store_blob WHERE id=${sqlQuote(boundBlob)}`,
      ),
      1,
      'logical denial does not remove bytes while an intrinsic reference remains',
    );
    assert.equal(
      (await json('POST', scoped(WS_A, `sessions/${boundSession.body.id}/archive`))).status,
      200,
    );
    const boundReceipt = await waitReceipt(directory, 'file', boundFile, { workspace: WS_A });
    assert.equal(boundReceipt.receipt.evidence.blob_deleted, true);
    const repositoryId = `managed:${boundSession.body.id}:repository:0`;
    const repositoryReceipt = await waitReceipt(directory, 'repository', repositoryId);
    assert.equal(repositoryReceipt.receipt.evidence.local_realizations_deleted, 0);

    // Equal bytes share one blob. Removing A cannot delete bytes still owned
    // to B; revoking B subsequently permits physical GC.
    const sharedA = await upload(WS_A, 'shared-content');
    const sharedB = await upload(WS_B, 'shared-content');
    const sharedBlob = blobForFile(directory, WS_A, sharedA);
    assert.notEqual(sharedA, sharedB, 'workspace ownership uses distinct logical File ids');
    assert.equal(
      sharedBlob,
      blobForFile(directory, WS_B, sharedB),
      'equal bytes reuse the one content-addressed physical blob',
    );
    assert.equal((await json('DELETE', scoped(WS_A, `files/${sharedA}`))).status, 200);
    await sleep(5_500);
    assert.equal((await json('GET', scoped(WS_B, `files/${sharedB}`))).status, 200);
    assert.equal(
      scalar(
        path.join(directory, 'files.db'),
        `SELECT count(*) FROM file_store_blob WHERE id=${sqlQuote(sharedBlob)}`,
      ),
      1,
      'the remaining logical owner retains the shared physical blob',
    );
    assert.equal((await json('DELETE', scoped(WS_B, `files/${sharedB}`))).status, 200);
    await waitReceipt(directory, 'file', sharedB, { workspace: WS_B });

    // Memory purge removes live heads and the version log from the same canonical
    // repository only after the store tombstone is visible.
    const memoryStore = await json('POST', scoped(WS_A, 'memory_stores'), { name: 'reclaim-me' });
    assert.equal(memoryStore.status, 200);
    const memoryId = memoryStore.body.id;
    assert.equal((await json('POST', scoped(WS_A, `memory_stores/${memoryId}/memories`), {
      path: '/fact.md', content: 'remember',
    })).status, 200);
    assert.equal((await json('DELETE', scoped(WS_A, `memory_stores/${memoryId}`))).status, 200);
    const memoryReceipt = await waitReceipt(directory, 'memory_store', memoryId);
    assert.equal(memoryReceipt.receipt.evidence.heads_deleted, 1);
    assert.equal(memoryReceipt.receipt.evidence.versions_deleted, 1);

    // Skill delete hides new resolution immediately, then removes the retained
    // immutable bundle once no Session/Agent binding remains.
    const skill = await json('POST', scoped(WS_A, 'skills'), {
      id: 'reclaim-skill',
      content: '---\nname: reclaim-skill\ndescription: test\n---\nUse safely.',
    });
    assert.equal(skill.status, 200);
    const skillId = skill.body.id;
    assert.equal((await json('DELETE', scoped(WS_A, `skills/${skillId}`))).status, 200);
    assert.equal((await json('GET', scoped(WS_A, `skills/${skillId}`))).status, 404);
    const skillReceipt = await waitReceipt(directory, 'skill', skillId);
    assert.equal(skillReceipt.receipt.evidence.versions_deleted, 1);

    console.log('E2E PASS: durable resource deny, crash recovery, reference guards, and per-kind receipts.');
  } finally {
    await stop(server).catch(() => {});
    fs.rmSync(directory, { recursive: true, force: true });
  }
}

main().catch((error) => {
  console.error('E2E FAIL:', error);
  process.exitCode = 1;
});
