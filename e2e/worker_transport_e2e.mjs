// The cross-node worker HTTP surface, end to end over the real server binary.
//
// A database-less worker drives runs and commits facts over HTTP — never opening
// the store. This exercises the two worker-facing seams the server now mounts:
//   - commit ingest  (POST /v1/worker/commit): the worker pushes a ThreadCommit,
//     the server (single writer) applies it; the fact reads back from the store,
//     and a redelivery is idempotent (at-least-once -> exactly-once effect).
//   - dispatch transport (POST /v1/worker/dispatch/claim): a worker claims runs
//     from the shared durable queue over HTTP; the endpoint is live and wired to
//     the real store (full enqueue->claim->settle semantics are proven in Rust).
//
// Run: node e2e/worker_transport_e2e.mjs

import assert from 'node:assert/strict';
import { mkdtempSync } from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { spawnServer, stopServer, waitForPort, pass } from './harness.mjs';

const PORT = Number(process.env.E2E_PORT ?? 38812);
const BASE = `http://127.0.0.1:${PORT}`;
const THREAD = 'worker-transport-1';
const ENV = {
  AWAKEN_INGRESS: 'durable',
  AWAKEN_STORAGE_DIR: mkdtempSync(path.join(tmpdir(), 'awaken-worker-transport-')),
};

// The exact ThreadCommit wire shape (dumped from the neutral Rust types).
function threadCommit() {
  return {
    thread_id: THREAD,
    run_fact: { run_id: 'run-A', phase: { Ended: 'NaturalEnd' } },
    messages: [
      { id: 'a1', role: 'Assistant', content: [{ type: 'text', text: 'hi from a db-less worker' }] },
    ],
    state: [],
    events: [],
    waiting: null,
  };
}

async function postJson(pathname, body) {
  const res = await fetch(`${BASE}${pathname}`, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify(body),
  });
  const text = await res.text();
  let json;
  try {
    json = JSON.parse(text);
  } catch {
    json = null;
  }
  return { status: res.status, json, text };
}

async function threadMessages() {
  const res = await fetch(`${BASE}/v1/durable/threads/${THREAD}/messages`);
  assert.equal(res.status, 200, 'durable thread messages readable');
  return (await res.json()).messages ?? [];
}

async function main() {
  const { server } = spawnServer('echo', PORT, ENV);
  try {
    await waitForPort(PORT);

    // --- commit ingest: a db-less worker pushes facts; the server commits them ---
    const first = await postJson('/v1/worker/commit', threadCommit());
    assert.equal(first.status, 200, `commit ingest accepted: ${first.text}`);
    assert.ok(
      typeof first.json?.sequence === 'number',
      `the server returns a CommitRecord with a sequence: ${first.text}`,
    );

    const committed = await threadMessages();
    const mine = committed.filter((m) => (m.text ?? '').includes('db-less worker'));
    assert.equal(mine.length, 1, `the worker's fact committed on the server: ${JSON.stringify(committed)}`);
    pass('commit ingest: a db-less worker pushed a fact and the server committed it (readable back)');

    // --- idempotent redelivery: at-least-once retry is a no-op, not a duplicate ---
    const again = await postJson('/v1/worker/commit', threadCommit());
    assert.equal(again.status, 200, `redelivered commit accepted: ${again.text}`);
    const afterRedeliver = await threadMessages();
    const stillMine = afterRedeliver.filter((m) => (m.text ?? '').includes('db-less worker'));
    assert.equal(
      stillMine.length,
      1,
      `a redelivered commit does not duplicate: ${JSON.stringify(afterRedeliver)}`,
    );
    pass('commit ingest: an at-least-once redelivery is idempotent (no duplicate)');

    // --- dispatch transport: the claim endpoint is live and wired to the store ---
    const claim = await postJson('/v1/worker/dispatch/claim', {
      owner: 'ts-worker-1',
      lease_ms: 30_000,
      now_ms: Date.now(),
    });
    assert.equal(claim.status, 200, `dispatch claim endpoint live: ${claim.text}`);
    assert.ok('claimed' in (claim.json ?? {}), `claim returns the wire shape: ${claim.text}`);
    // Settling an unknown run is a tolerated no-op (the worker's settle path is live).
    const settle = await postJson('/v1/worker/dispatch/settle', {
      run_id: 'no-such-run',
      outcome: 'Done',
      consumed: [],
    });
    assert.equal(settle.status, 200, `dispatch settle endpoint live: ${settle.text}`);
    pass('dispatch transport: worker claim/settle endpoints are live over HTTP on the real server');
  } finally {
    await stopServer(server);
  }

  console.log('\nE2E PASS: the cross-node db-less worker HTTP surface (commit ingest + dispatch transport) works over the real server.');
}

main().catch((err) => {
  console.error('E2E FAIL:', err);
  process.exitCode = 1;
});
