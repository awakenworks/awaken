// The cross-node db-less worker, POOL-DRIVEN, over two real processes.
//
//   - A coordinator-only cell server (AWAKEN_DISABLE_LOCAL_POOL=1): owns the
//     durable queue + store, serves HTTP, but runs NO local pool — so it never
//     drives runs itself.
//   - A database-less worker (AWAKEN_UPSTREAM_URL=<server>): its dispatch pool
//     claims runs from the server over the transport, drives them (echo model),
//     and commits the facts back to the server — holding no store, serving no HTTP.
//
// A background run submitted to the server is therefore driven EXCLUSIVELY by the
// remote worker, and its committed reply reads back from the server's store. This
// proves the full pool-driven cross-node worker path end to end.
//
// Run: node e2e/worker_pool_e2e.mjs

import assert from 'node:assert/strict';
import { mkdtempSync } from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { spawnServer, stopServer, waitForPort, pass } from './harness.mjs';

const SERVER_PORT = Number(process.env.E2E_PORT ?? 38833);
const SERVER = `http://127.0.0.1:${SERVER_PORT}`;
const THREAD = 'worker-pool-1';
const STORAGE = mkdtempSync(path.join(tmpdir(), 'awaken-worker-pool-'));

async function submitBackground(text) {
  const res = await fetch(`${SERVER}/v1/durable/threads/${THREAD}/submit_background`, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify({ text }),
  });
  const body = await res.text();
  assert.equal(res.status, 200, `submit_background: ${body}`);
  return JSON.parse(body).run_id;
}

async function assistantReplies() {
  const res = await fetch(`${SERVER}/v1/durable/threads/${THREAD}/messages`);
  if (res.status !== 200) return [];
  const msgs = (await res.json()).messages ?? [];
  return msgs.filter((m) => m.role === 'Assistant' && (m.text ?? '').length > 0);
}

async function waitForWorkerReply(timeoutMs = 25_000) {
  const deadline = Date.now() + timeoutMs;
  for (;;) {
    const replies = await assistantReplies();
    if (replies.length >= 1) return replies;
    if (Date.now() > deadline) throw new Error('timed out waiting for the remote worker to drive the run');
    await new Promise((r) => setTimeout(r, 200));
  }
}

async function main() {
  // Coordinator-only server: durable store + HTTP, but no local pool.
  const { server } = spawnServer('echo', SERVER_PORT, {
    AWAKEN_INGRESS: 'durable',
    AWAKEN_STORAGE_DIR: STORAGE,
    AWAKEN_DISABLE_LOCAL_POOL: '1',
  });
  // Database-less worker: drains the server's queue over HTTP (no port of its own).
  const { server: worker } = spawnServer('echo', 0, {
    AWAKEN_INGRESS: 'durable',
    AWAKEN_UPSTREAM_URL: SERVER,
    AWAKEN_HTTP_ADDR: '127.0.0.1:0',
  });

  try {
    await waitForPort(SERVER_PORT);
    // Give the worker a moment to start its draining pool.
    await new Promise((r) => setTimeout(r, 2_000));

    // Submit a background run to the server. The server has no pool, so ONLY the
    // remote worker can drive it.
    await submitBackground('drive me from a db-less worker');

    const replies = await waitForWorkerReply();
    assert.ok(
      replies.some((m) => (m.text ?? '').includes('drive me from a db-less worker')),
      `the remote worker drove the run and committed the echo reply: ${JSON.stringify(replies)}`,
    );
    pass('pool-driven cross-node worker: a coordinator-only server enqueued a run, a db-less worker claimed/drove/committed it, the reply read back from the server');
  } finally {
    await stopServer(worker);
    await stopServer(server);
  }

  console.log('\nE2E PASS: the full pool-driven cross-node db-less worker path works over two real processes.');
}

main().catch((err) => {
  console.error('E2E FAIL:', err);
  process.exitCode = 1;
});
