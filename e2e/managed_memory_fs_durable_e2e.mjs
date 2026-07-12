// Durable path-addressed memory store (ADR-0053) over HTTP, across a real restart.
//
// The `/v1/memory_stores/:id/memories` endpoints are backed by the durable,
// path-addressed `MemoryFs` (content_sha256 + compare-and-swap) — the same store a
// write-through FUSE mount projects. This drives that store end-to-end through the
// real server binary: create path-addressed memories, exercise the CAS precondition
// (stale → 409, fresh → ok, version bumped), path_prefix listing, then KILL the
// server and start a fresh one over the SAME storage dir. The memories must survive —
// they live in the durable store of record, not an in-memory registry.
//
// Deterministic (`echo` mode; the memory API is model-independent), so it runs in CI
// without an API key.
//
// Run: (from e2e/)  node managed_memory_fs_durable_e2e.mjs

import assert from 'node:assert/strict';
import fs from 'node:fs';
import Anthropic from '@anthropic-ai/sdk';
import { spawnServer, stopServer, waitForPort, pass } from './harness.mjs';

const PORT = Number(process.env.E2E_PORT ?? 38513);
const BETAS = ['managed-agents-2026-04-01'];
const STORE_DIR = `/tmp/awaken-memfs-durable-e2e-${process.pid}`;

const client = () => new Anthropic({ apiKey: 'e2e-dummy', baseURL: `http://127.0.0.1:${PORT}` });
const drain = async (p) => {
  const out = [];
  for await (const x of p) out.push(x);
  return out;
};

async function main() {
  fs.rmSync(STORE_DIR, { recursive: true, force: true });
  let { server } = spawnServer('echo', PORT, { AWAKEN_STORAGE_DIR: STORE_DIR });
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
    pass('compare-and-swap update: stale 409, fresh ok, version bumped');

    // -- path_prefix drills into a subtree ------------------------------------
    const drilled = await drain(
      c.beta.memoryStores.memories.list(store.id, { path_prefix: '/archive', betas: BETAS }),
    );
    assert.deepEqual(
      drilled.map((m) => m.path),
      ['/archive/old.md'],
      'path_prefix returns only memories under the prefix',
    );
    pass('path_prefix listing');

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

    // -- RESTART over the same storage dir ------------------------------------
    await stopServer(server);
    ({ server } = spawnServer('echo', PORT, { AWAKEN_STORAGE_DIR: STORE_DIR }));
    await waitForPort(PORT);
    c = client();

    // The memories survive the process death — they were in the durable store of
    // record, not an in-memory registry.
    const after = await drain(c.beta.memoryStores.memories.list(store.id, { betas: BETAS }));
    const byPath = Object.fromEntries(after.map((m) => [m.path, m.content]));
    assert.equal(byPath['/notes/a.md'], 'second', 'the CAS-updated memory survived the restart');
    assert.equal(byPath['/archive/old.md'], 'kept', 'the second memory survived the restart');
    pass('path-addressed memories are durable across a process restart');

    console.log('E2E PASS: durable path-addressed memory store (create + retrieve + CAS + restart).');
  } finally {
    stopServer(server);
    fs.rmSync(STORE_DIR, { recursive: true, force: true });
  }
}

main().catch((err) => {
  console.error('E2E FAIL:', err);
  process.exit(1);
});
