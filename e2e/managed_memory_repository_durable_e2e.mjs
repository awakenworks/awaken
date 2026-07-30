// Durable path-addressed memory store (ADR-0053) over HTTP, across a real restart.
//
// The `/v1/memory_stores/:id/memories` endpoints are backed by the durable,
// path-addressed `MemoryRepository` (content_sha256 + compare-and-swap) — the same store a
// write-through FUSE mount projects. This drives that store end-to-end through the
// real server binary: create path-addressed memories, exercise the CAS precondition
// (stale → 409, fresh → ok, version bumped), path_prefix listing, then KILL the
// server and start a fresh one over the SAME storage dir. The memories must survive —
// they live in the durable store of record, not an in-memory registry.
//
// Deterministic (`echo` mode; the memory API is model-independent), so it runs in CI
// without an API key.
//
// Run: (from e2e/)  node managed_memory_repository_durable_e2e.mjs

import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { DatabaseSync } from 'node:sqlite';
import Anthropic from '@anthropic-ai/sdk';
import { spawnServer, stopServer, waitForPort, pass } from './harness.mjs';

const PORT = Number(process.env.E2E_PORT ?? 38513);
const BETAS = ['agent-memory-2026-07-22'];
const STORE_DIR = path.join(os.tmpdir(), `awaken-memory-repository-durable-e2e-${process.pid}`);

const client = () => new Anthropic({
  apiKey: 'e2e-dummy',
  baseURL: `http://127.0.0.1:${PORT}`,
  defaultHeaders: { 'anthropic-beta': BETAS[0] },
});
const drain = async (p) => {
  const out = [];
  for await (const x of p) out.push(x);
  return out;
};

async function rejectsStatus(operation, status, message) {
  await assert.rejects(operation, (error) => error.status === status, message);
}

function sqliteQueryOne(database, sql) {
  const connection = new DatabaseSync(database);
  try {
    return connection.prepare(sql).get();
  } finally {
    connection.close();
  }
}

function sqliteExec(database, sql) {
  const connection = new DatabaseSync(database);
  try {
    connection.exec(sql);
  } finally {
    connection.close();
  }
}

async function main() {
  fs.rmSync(STORE_DIR, { recursive: true, force: true });
  let { server } = spawnServer('echo', PORT, { SESSION_DEPLOYMENT_STORAGE_DIR: STORE_DIR });
  try {
    await waitForPort(PORT);
    let c = client();

    // -- create path-addressed memories in the durable store ------------------
    const store = await c.beta.memoryStores.create({ betas: BETAS });
    const mem = await c.beta.memoryStores.memories.create(store.id, {
      path: '/notes/a.md',
      content: 'first',
      betas: BETAS,
    });
    assert.equal(mem.type, 'memory');
    assert.equal(mem.path, '/notes/a.md');
    assert.equal(mem.content_size_bytes, 5);
    assert.equal(mem.content_sha256.length, 64, 'content_sha256 is a 64-char hex digest');
    await c.beta.memoryStores.memories.create(store.id, {
      path: '/archive/old.md',
      content: 'kept',
      betas: BETAS,
    });
    await c.beta.memoryStores.memories.create(store.id, {
      path: '/archive/deep/nested.md',
      content: 'nested',
      betas: BETAS,
    });
    // retrieve by id round-trips the head content.
    const got = await c.beta.memoryStores.memories.retrieve(mem.id, {
      memory_store_id: store.id,
      betas: BETAS,
    });
    assert.equal(got.content, 'first');
    pass('path-addressed memories create + retrieve via the durable store');

    // -- CAS: stale precondition -> 409, fresh -> ok, version bumps -----------
    await assert.rejects(
      () =>
        c.beta.memoryStores.memories.update(mem.id, {
          memory_store_id: store.id,
          content: 'nope',
          precondition: { type: 'content_sha256', content_sha256: 'deadbeef'.repeat(8) },
          betas: BETAS,
        }),
      (e) => e.status === 409,
      'a stale content_sha256 precondition is rejected 409',
    );
    const up = await c.beta.memoryStores.memories.update(mem.id, {
      memory_store_id: store.id,
      content: 'second',
      precondition: { type: 'content_sha256', content_sha256: mem.content_sha256 },
      betas: BETAS,
    });
    assert.equal(up.content, 'second');
    assert.notEqual(up.memory_version_id, mem.memory_version_id, 'an update mints a new version');
    const staleReplay = await c.beta.memoryStores.memories.update(mem.id, {
      memory_store_id: store.id,
      content: 'second',
      precondition: { type: 'content_sha256', content_sha256: mem.content_sha256 },
      betas: BETAS,
    });
    assert.equal(staleReplay.memory_version_id, up.memory_version_id);
    const freshReplay = await c.beta.memoryStores.memories.update(mem.id, {
      memory_store_id: store.id,
      content: 'second',
      precondition: { type: 'content_sha256', content_sha256: up.content_sha256 },
      betas: BETAS,
    });
    assert.equal(freshReplay.memory_version_id, up.memory_version_id);
    const displaced = await c.beta.memoryStores.memories.create(store.id, {
      path: '/notes/replaced.md',
      content: 'displaced',
      betas: BETAS,
    });
    const moved = await c.beta.memoryStores.memories.update(mem.id, {
      memory_store_id: store.id,
      path: '/notes/replaced.md',
      content: 'second',
      precondition: { type: 'content_sha256', content_sha256: up.content_sha256 },
      betas: BETAS,
    });
    assert.equal(moved.path, '/notes/replaced.md');
    await assert.rejects(
      () => c.beta.memoryStores.memories.retrieve(displaced.id, {
        memory_store_id: store.id,
        betas: BETAS,
      }),
      (error) => error.status === 404,
      'moving onto an occupied path records deletion of the displaced head',
    );
    pass('compare-and-swap update: stale 409, fresh ok, version bumped');

    // Causes: segment-aligned path_prefix, depth 0/1, basic/full projection,
    // page limit/cursor, and invalid prefix/depth values.
    // Constraints: prefix ends `/`; depth is only 0 or 1; full pages cap at 20.
    // Effects: recursive memories or immediate memory_prefix rollups in stable path
    // order, content only in full view, and 400 before repository reads on invalid input.
    // Decision rule: Memory-list ML1-ML7.
    const drilled = await drain(c.beta.memoryStores.memories.list(store.id, {
      path_prefix: '/archive/', depth: 0, betas: BETAS,
    }));
    assert.deepEqual(
      drilled.map((m) => m.path),
      ['/archive/deep/nested.md', '/archive/old.md'],
      'ML1 path_prefix returns the recursive subtree in stable path order',
    );
    assert.ok(drilled.every((memory) => memory.content == null), 'ML2 list defaults to basic');
    const shallow = await drain(c.beta.memoryStores.memories.list(store.id, {
      path_prefix: '/archive/', depth: 1, betas: BETAS,
    }));
    assert.deepEqual(
      shallow.map((item) => [item.type, item.path]),
      [['memory_prefix', '/archive/deep/'], ['memory', '/archive/old.md']],
      'ML3 depth=1 rolls deeper paths into one prefix marker',
    );
    const full = await drain(c.beta.memoryStores.memories.list(store.id, {
      path_prefix: '/archive/', depth: 0, view: 'full', limit: 100, betas: BETAS,
    }));
    assert.deepEqual(full.map((memory) => memory.content), ['nested', 'kept'], 'ML4 full populates content');
    await rejectsStatus(
      () => c.beta.memoryStores.memories.list(store.id, { path_prefix: '/archive', betas: BETAS }),
      400,
      'ML5 non-segment-aligned prefix rejects',
    );
    await rejectsStatus(
      () => c.beta.memoryStores.memories.list(store.id, { path_prefix: '/', depth: 2, betas: BETAS }),
      400,
      'ML6 unsupported depth rejects',
    );
    pass('ML1-ML6 path/depth/view Memory listing contract');

    // -- error paths: invalid path 400, unknown memory 404, delete then 404 ---
    await assert.rejects(
      () =>
        c.beta.memoryStores.memories.create(store.id, {
          path: 'relative.md',
          content: 'x',
          betas: BETAS,
        }),
      (e) => e.status === 400,
      'a non-absolute path is rejected 400',
    );
    await assert.rejects(
      () =>
        c.beta.memoryStores.memories.retrieve('mem_does_not_exist', {
          memory_store_id: store.id,
          betas: BETAS,
        }),
      (e) => e.status === 404,
      'an unknown memory id is 404',
    );
    const tmp = await c.beta.memoryStores.memories.create(store.id, {
      path: '/tmp.md',
      content: 'ephemeral',
      betas: BETAS,
    });
    await c.beta.memoryStores.memories.delete(tmp.id, { memory_store_id: store.id, betas: BETAS });
    await assert.rejects(
      () =>
        c.beta.memoryStores.memories.retrieve(tmp.id, {
          memory_store_id: store.id,
          betas: BETAS,
        }),
      (e) => e.status === 404,
      'a deleted memory is 404',
    );
    pass('error paths: 400 invalid path, 404 unknown, 404 after delete');

    // -- complete public error graph -----------------------------------------
    const oversized = 'x'.repeat(102_401);
    await rejectsStatus(
      () => c.beta.memoryStores.memories.create(store.id, {
        path: '/too-large.md', content: oversized, betas: BETAS,
      }),
      400,
      'oversized Memory creation is rejected',
    );
    await rejectsStatus(
      () => c.beta.memoryStores.memories.update(mem.id, {
        memory_store_id: store.id, content: oversized, betas: BETAS,
      }),
      400,
      'oversized Memory update is rejected',
    );
    await rejectsStatus(
      () => c.beta.memoryStores.memories.update(mem.id, {
        memory_store_id: store.id, path: '../escape.md', betas: BETAS,
      }),
      400,
      'unsafe Memory rename is rejected',
    );
    await rejectsStatus(
      () => c.beta.memoryStores.memories.update('mem_does_not_exist', {
        memory_store_id: store.id, content: 'missing', betas: BETAS,
      }),
      404,
      'unknown Memory update is 404',
    );
    await rejectsStatus(
      () => c.beta.memoryStores.memories.delete('mem_does_not_exist', {
        memory_store_id: store.id, betas: BETAS,
      }),
      404,
      'unknown Memory delete is 404',
    );

    const versions = await drain(c.beta.memoryStores.memoryVersions.list(store.id, {
      betas: BETAS,
    }));
    assert.ok(versions.length > 0);
    await rejectsStatus(
      () => c.get(`/v1/memory_stores/${store.id}/memory_versions/memver_missing`),
      404,
      'unknown Memory version is 404',
    );
    await rejectsStatus(
      () => c.post(`/v1/memory_stores/${store.id}/memory_versions/memver_missing/redact`),
      404,
      'unknown Memory version redaction is 404',
    );
    await rejectsStatus(
      () => c.get(`/v1/memory_stores/${store.id}/config_versions/not-an-integer`),
      400,
      'non-integer config version is rejected',
    );
    await rejectsStatus(
      () => c.get(`/v1/memory_stores/${store.id}/config_versions/999`),
      404,
      'unknown config version is 404',
    );
    await rejectsStatus(
      () => c.post(`/v1/memory_stores/${store.id}/config`, { body: {} }),
      400,
      'config publication requires a CAS version',
    );
    await rejectsStatus(
      () => c.post(`/v1/memory_stores/${store.id}/config`, {
        body: { expected_config_version: 1 },
      }),
      400,
      'config publication requires a policy change',
    );
    for (const [field, value] of [
      ['recall_policy', { enabled: 'yes' }],
      ['extraction_policy', { enabled: 'yes' }],
      ['retention_policy', { retention_days: 'forever' }],
    ]) {
      await rejectsStatus(
        () => c.post(`/v1/memory_stores/${store.id}/config`, {
          body: { expected_config_version: 1, [field]: value },
        }),
        400,
        `${field} must preserve its typed schema`,
      );
    }

    const missingStore = 'memstore_does_not_exist';
    const missingStoreOperations = [
      () => c.get(`/v1/memory_stores/${missingStore}`),
      () => c.post(`/v1/memory_stores/${missingStore}`, { body: { description: 'missing' } }),
      () => c.get(`/v1/memory_stores/${missingStore}/config`),
      () => c.get(`/v1/memory_stores/${missingStore}/config_versions/1`),
      () => c.post(`/v1/memory_stores/${missingStore}/archive`),
      () => c.delete(`/v1/memory_stores/${missingStore}`),
      () => c.get(`/v1/memory_stores/${missingStore}/memories`),
      () => c.post(`/v1/memory_stores/${missingStore}/memories`, {
        body: { path: '/missing.md', content: 'missing' },
      }),
      () => c.get(`/v1/memory_stores/${missingStore}/memories/mem_missing`),
      () => c.post(`/v1/memory_stores/${missingStore}/memories/mem_missing`, {
        body: { content: 'missing' },
      }),
      () => c.delete(`/v1/memory_stores/${missingStore}/memories/mem_missing`),
      () => c.get(`/v1/memory_stores/${missingStore}/memory_versions`),
      () => c.get(`/v1/memory_stores/${missingStore}/memory_versions/memver_missing`),
      () => c.post(`/v1/memory_stores/${missingStore}/memory_versions/memver_missing/redact`),
    ];
    for (const operation of missingStoreOperations) {
      await rejectsStatus(operation, 404, 'a nested operation cannot invent a missing store');
    }

    const archived = await c.beta.memoryStores.create({ name: 'archived', betas: BETAS });
    await c.post(`/v1/memory_stores/${archived.id}/archive`);
    await rejectsStatus(
      () => c.post(`/v1/memory_stores/${archived.id}/memories`, {
        body: { path: '/after-archive.md', content: 'forbidden' },
      }),
      404,
      'an archived MemoryStore cannot accept mutable content',
    );
    pass('public Memory error graph fails closed at every aggregate boundary');

    // -- RESTART over the same storage dir ------------------------------------
    await stopServer(server);
    ({ server } = spawnServer('echo', PORT, { SESSION_DEPLOYMENT_STORAGE_DIR: STORE_DIR }));
    await waitForPort(PORT);
    c = client();

    // The memories survive the process death — they were in the durable store of
    // record, not an in-memory registry.
    const after = await drain(c.beta.memoryStores.memories.list(store.id, { view: 'full', betas: BETAS }));
    const byPath = Object.fromEntries(after.map((m) => [m.path, m.content]));
    assert.equal(
      byPath['/notes/replaced.md'],
      'second',
      'the moved CAS-updated memory survived the restart',
    );
    assert.equal(byPath['/archive/old.md'], 'kept', 'the second memory survived the restart');
    pass('path-addressed memories are durable across a process restart');

    // Persistence corruption must fail closed through the public resource API.
    // These mutations emulate damaged durable rows; no test-only service route is
    // involved, and each row is restored before checking the next decoder arm.
    const database = `${STORE_DIR}/memory_fs.db`;
    const modifiedVersion = sqliteQueryOne(
      database,
      `SELECT id FROM memory_store_versions WHERE store_id='${store.id}' AND operation='modified' ORDER BY ordinal DESC LIMIT 1`,
    ).id;
    sqliteExec(
      database,
      `UPDATE memory_store_versions SET operation='corrupt-operation' WHERE id='${modifiedVersion}'`,
    );
    await assert.rejects(
      () => drain(c.beta.memoryStores.memoryVersions.list(store.id, { betas: BETAS })),
      (error) => error.status === 500,
      'an unknown durable operation is never projected as a valid version',
    );
    sqliteExec(
      database,
      `UPDATE memory_store_versions SET operation='modified' WHERE id='${modifiedVersion}'`,
    );
    sqliteExec(
      database,
      `UPDATE memory_store_memories SET content=X'FFFE' WHERE id='${mem.id}'`,
    );
    await assert.rejects(
      () => c.beta.memoryStores.memories.retrieve(mem.id, {
        memory_store_id: store.id,
        betas: BETAS,
      }),
      (error) => error.status === 404 || error.status === 500,
      'non-UTF8 Memory content fails closed',
    );
    sqliteExec(
      database,
      `UPDATE memory_store_memories SET content=CAST('second' AS BLOB) WHERE id='${mem.id}'`,
    );
    sqliteExec(
      database,
      `UPDATE memory_store_versions SET content=X'FFFE' WHERE id='${modifiedVersion}'`,
    );
    await assert.rejects(
      () => drain(c.beta.memoryStores.memoryVersions.list(store.id, { betas: BETAS })),
      (error) => error.status === 500,
      'non-UTF8 version content fails closed',
    );
    sqliteExec(
      database,
      `UPDATE memory_store_versions SET content=CAST('second' AS BLOB) WHERE id='${modifiedVersion}'`,
    );
    pass('corrupt durable Memory/version rows fail closed without fabricating state');

    console.log('E2E PASS: durable path-addressed memory store (create + retrieve + CAS + restart).');
  } finally {
    // Cause: SQLite retains the file handle until the child has actually exited;
    // effect: await shutdown, then tolerate short Windows scanner/FS lock delays.
    await stopServer(server);
    fs.rmSync(STORE_DIR, { recursive: true, force: true, maxRetries: 10, retryDelay: 100 });
  }
}

main().catch((err) => {
  console.error('E2E FAIL:', err);
  process.exit(1);
});
