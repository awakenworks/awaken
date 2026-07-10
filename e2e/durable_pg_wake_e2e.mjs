// Durable dispatch pool on Postgres with the cross-node pg_notify wake (B-P2),
// end to end over the real server binary.
//
// Proves the served durable path when the backend is Postgres AND
// AWAKEN_DISPATCH_WAKE=pg-notify: the process pool is spawned with a PgNotifyWake
// (not the in-process LocalWakeSignal), a background-submitted run is driven
// autonomously by the pool over the shared Postgres queue, its result is committed,
// and the history survives a process restart against the same database. The
// migrations (incl. V0011 `sandbox`) apply on connect. The cross-node wake LATENCY
// guarantee (a peer's enqueue nudges a blocked pool) is proven deterministically by
// the Rust component test crates/agents/awaken-run-ingress/tests/pg_notify_wake.rs;
// this e2e proves the served pool constructs and drives through PgNotifyWake over a
// real Postgres queue via the HTTP surface.
//
// Requires AWAKEN_DATABASE_URL (a reachable Postgres). Run:
//   AWAKEN_DATABASE_URL=postgres://postgres:test@127.0.0.1:5432/awaken \
//     node e2e/durable_pg_wake_e2e.mjs

import { mkdtempSync } from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { spawnServer, stopServer, waitForPort, pass } from './harness.mjs';

const PORT = Number(process.env.E2E_PORT ?? 38795);
const THREAD = 'durable-pg-wake-1';
const BASE = `http://127.0.0.1:${PORT}`;

const DB_URL = process.env.AWAKEN_DATABASE_URL;
if (!DB_URL) {
  console.error('SKIP: durable_pg_wake_e2e requires AWAKEN_DATABASE_URL');
  process.exit(0);
}

// Durable ingress with the dispatch QUEUE on Postgres (+ cross-node pg_notify wake).
// The run commit history uses AWAKEN_STORAGE_DIR (the commit store), kept on a
// persistent dir so it — like the Postgres queue — survives a process restart.
const ENV = {
  AWAKEN_INGRESS: 'durable',
  AWAKEN_DISPATCH_BACKEND: 'postgres',
  AWAKEN_DATABASE_URL: DB_URL,
  AWAKEN_DISPATCH_WAKE: 'pg-notify',
  AWAKEN_DISPATCH_WAKE_CHANNEL: 'awaken_dispatch_wake',
  AWAKEN_STORAGE_DIR: mkdtempSync(path.join(tmpdir(), 'awaken-pg-wake-')),
};

async function submitBackground(text) {
  const res = await fetch(`${BASE}/v1/durable/threads/${THREAD}/submit_background`, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify({ text }),
  });
  if (res.status !== 200) throw new Error(`submit_background ${res.status}: ${await res.text()}`);
  const body = await res.json();
  if (!body.run_id || body.queued !== true) throw new Error(`unexpected submit body: ${JSON.stringify(body)}`);
  return body.run_id;
}

async function messages() {
  const res = await fetch(`${BASE}/v1/durable/threads/${THREAD}/messages`);
  if (res.status !== 200) throw new Error(`messages ${res.status}: ${await res.text()}`);
  return (await res.json()).messages ?? [];
}

// Poll committed truth until the pool has driven the run and an assistant reply is
// committed — the caller observes durable completion, it does not drive the run.
async function waitForAssistant(minCount, timeoutMs = 30_000) {
  const deadline = Date.now() + timeoutMs;
  for (;;) {
    const msgs = await messages();
    const assistants = msgs.filter((m) => m.role === 'Assistant' && (m.text ?? '').length > 0);
    if (assistants.length >= minCount) return { msgs, assistants };
    if (Date.now() > deadline) {
      throw new Error(`timed out waiting for ${minCount} assistant reply(ies); saw ${JSON.stringify(msgs)}`);
    }
    await new Promise((r) => setTimeout(r, 150));
  }
}

async function main() {
  // 1. First process: the pool is spawned with PgNotifyWake over the shared Postgres
  //    queue; a background run is driven autonomously by that pool.
  let { server } = spawnServer('echo', PORT, ENV);
  try {
    await waitForPort(PORT);
    const runId = await submitBackground('hello pg wake');
    pass(`background run queued (${runId}) over the Postgres queue — the pg-notify-wired pool will drive it`);

    const { assistants } = await waitForAssistant(1);
    pass(`the pool (spawned with PgNotifyWake) drove the durable run to completion (reply: ${JSON.stringify(assistants[0].text)})`);
  } finally {
    await stopServer(server);
  }
  pass('first process stopped — the run and its result live only in the Postgres store');

  // 2. Restart against the SAME database: committed history survives, and the
  //    pg-notify-wired pool keeps draining a fresh submission.
  ({ server } = spawnServer('echo', PORT, ENV));
  try {
    await waitForPort(PORT);
    const survived = await messages();
    if (!survived.some((m) => m.role === 'Assistant' && (m.text ?? '').length > 0)) {
      throw new Error(`durable history did not survive the restart: ${JSON.stringify(survived)}`);
    }
    pass('committed history survived the process restart (Postgres store)');

    await submitBackground('after restart');
    const { assistants } = await waitForAssistant(2);
    pass(`the pg-notify-wired pool drained a fresh run after the restart (${assistants.length} assistant replies committed)`);
  } finally {
    await stopServer(server);
  }

  console.log('\nDURABLE PG WAKE E2E PASS: background runs driven by the process pool over a shared Postgres queue with the pg_notify cross-node wake, surviving a restart.');
}

main().catch((err) => {
  console.error(`\nDURABLE PG WAKE E2E FAIL: ${err.stack ?? err}`);
  process.exit(1);
});
