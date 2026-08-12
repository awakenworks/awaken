// Cause graph (cross-node worker pool):
//   C1 preferred port is occupied    -> E1 select an available coordinator port
//   C2 coordinator exits pre-ready   -> E2 readiness fails with process status
//   C3 coordinator has no local pool -> E3 only the remote worker can claim
//   C4 remote worker is connected    -> E4 drive and commit exactly one reply
//   C5 shutdown is sequential        -> E5 worker/coordinator long-poll deadlock
//   C6 local pool is disabled         -> E6 expose the canonical private Worker
//                                          transport on the Scenario's sole listener
//
// Decision table:
//   Rule  C1  C2  C3  C4  C5  C6  Expected
//   T1    Y   N   -   -   -   -   E1
//   T2    -   Y   -   -   -   -   E2
//   T3    -   N   Y   Y   -   Y   E3 + E4 + E6
//   T4    -   N   Y   Y   N   -   stop both concurrently; no E5
//
// The cross-node db-less worker, POOL-DRIVEN, over two real processes.
//
//   - A coordinator-only cell server (`disable_local_pool` fixture axis): owns the
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
import { availablePort, spawnServer, stopServer, waitForPort, pass } from './harness.mjs';

const PREFERRED_SERVER_PORT = Number(process.env.E2E_PORT ?? 38833);
let SERVER_PORT;
let SERVER;
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
  SERVER_PORT = await availablePort(PREFERRED_SERVER_PORT);
  SERVER = `http://127.0.0.1:${SERVER_PORT}`;
  // Coordinator-only server: durable store + HTTP, but no local pool.
  const { server } = spawnServer('echo', SERVER_PORT, {
    SESSION_DEPLOYMENT_INGRESS: 'durable',
    SESSION_DEPLOYMENT_STORAGE_DIR: STORAGE,
    SESSION_DEPLOYMENT_DISABLE_LOCAL_POOL: '1',
  });
  let worker;

  try {
    await waitForPort(SERVER_PORT, 180_000, server);
    // Registration is an authority-changing boot operation, so start the worker
    // only after the coordinator is accepting requests (the Kubernetes deployment
    // obtains the same ordering through restart/readiness behavior).
    worker = spawnServer('echo', 0, {
      SESSION_DEPLOYMENT_INGRESS: 'durable',
      AWAKEN_UPSTREAM_URL: SERVER,
      AWAKEN_SCENARIO_ROLE: 'worker',
      AWAKEN_HTTP_ADDR: '127.0.0.1:0',
    }).server;
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
    assert.equal(replies.length, 1, 'the remote worker committed exactly one assistant reply');
    pass('pool-driven cross-node worker: a coordinator-only server enqueued a run, a db-less worker claimed/drove/committed it, the reply read back from the server');
  } finally {
    // The remote worker may have an in-flight long poll against the coordinator.
    // Closing the worker first can therefore wait for the still-live coordinator,
    // while closing the coordinator first can wait for that same client request.
    // Trigger both graceful shutdowns before awaiting either side.
    await Promise.all([
      worker ? stopServer(worker) : Promise.resolve(),
      stopServer(server),
    ]);
  }

  console.log('\nE2E PASS: the full pool-driven cross-node db-less worker path works over two real processes.');
}

main().catch((err) => {
  console.error('E2E FAIL:', err);
  process.exitCode = 1;
});
