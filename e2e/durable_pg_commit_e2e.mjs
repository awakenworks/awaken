// Shared Postgres commit backend (ADR-0022 D6): thread HISTORY on one Postgres, so
// any node warm-reloads any thread — not a per-thread local SQLite file that pins a
// thread to a node.
//
// Proves it end to end: run the server with AWAKEN_STORE=postgres, drive a durable
// run to a committed assistant reply, then restart the server with a DIFFERENT local
// storage dir. Because the thread history lives in Postgres (not the local dir), it
// survives — a per-thread SQLite file under the old dir could not.
//
// Requires AWAKEN_DATABASE_URL. Run:
//   AWAKEN_DATABASE_URL=postgres://postgres:test@127.0.0.1:5432/awaken \
//     node e2e/durable_pg_commit_e2e.mjs

import { mkdtempSync } from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { spawnServer, stopServer, waitForPort, pass } from './harness.mjs';

const PORT = Number(process.env.E2E_PORT ?? 38797);
const THREAD = 'pg-commit-1';
const BASE = `http://127.0.0.1:${PORT}`;

const DB_URL = process.env.AWAKEN_DATABASE_URL;
if (!DB_URL) {
  console.error('SKIP: durable_pg_commit_e2e requires AWAKEN_DATABASE_URL');
  process.exit(0);
}

// Dispatch queue AND commit history both on Postgres; the local storage dir holds
// nothing durable for this thread, so a restart with a fresh dir proves PG is the
// source of the surviving history.
function env(storageDir) {
  return {
    AWAKEN_INGRESS: 'durable',
    AWAKEN_DISPATCH_BACKEND: 'postgres',
    AWAKEN_STORE: 'postgres',
    AWAKEN_DATABASE_URL: DB_URL,
    AWAKEN_STORAGE_DIR: storageDir,
  };
}

async function submitBackground(text) {
  const res = await fetch(`${BASE}/v1/durable/threads/${THREAD}/submit_background`, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify({ text }),
  });
  if (res.status !== 200) throw new Error(`submit ${res.status}: ${await res.text()}`);
  const body = await res.json();
  if (!body.run_id || body.queued !== true) throw new Error(`unexpected submit body: ${JSON.stringify(body)}`);
  return body.run_id;
}

async function messages() {
  const res = await fetch(`${BASE}/v1/durable/threads/${THREAD}/messages`);
  if (res.status !== 200) throw new Error(`messages ${res.status}: ${await res.text()}`);
  return (await res.json()).messages ?? [];
}

async function waitForAssistant(minCount, timeoutMs = 30_000) {
  const deadline = Date.now() + timeoutMs;
  for (;;) {
    const msgs = await messages();
    const assistants = msgs.filter((m) => m.role === 'Assistant' && (m.text ?? '').length > 0);
    if (assistants.length >= minCount) return assistants;
    if (Date.now() > deadline) throw new Error(`timed out; saw ${JSON.stringify(msgs)}`);
    await new Promise((r) => setTimeout(r, 150));
  }
}

async function main() {
  // 1. First process: history committed to Postgres.
  let { server } = spawnServer('echo', PORT, env(mkdtempSync(path.join(tmpdir(), 'awaken-pgcommit-a-'))));
  try {
    await waitForPort(PORT);
    await submitBackground('remember this');
    const a = await waitForAssistant(1);
    pass(`run committed to the Postgres history (reply: ${JSON.stringify(a[0].text)})`);
  } finally {
    await stopServer(server);
  }

  // 2. Restart with a FRESH, DIFFERENT local storage dir. If the history were a
  //    per-thread local SQLite file it would be gone; from Postgres it survives.
  ({ server } = spawnServer('echo', PORT, env(mkdtempSync(path.join(tmpdir(), 'awaken-pgcommit-b-')))));
  try {
    await waitForPort(PORT);
    const survived = await messages();
    if (!survived.some((m) => m.role === 'Assistant' && (m.text ?? '').length > 0)) {
      throw new Error(`thread history did NOT survive the restart on a fresh dir: ${JSON.stringify(survived)}`);
    }
    pass('thread history survived a restart with a fresh local dir — it lives in Postgres, not a per-thread file');
  } finally {
    await stopServer(server);
  }

  console.log('\nDURABLE PG COMMIT E2E PASS: thread history committed to and warm-reloaded from shared Postgres across a restart on a different node dir.');
}

main().catch((err) => {
  console.error(`\nDURABLE PG COMMIT E2E FAIL: ${err.stack ?? err}`);
  process.exit(1);
});
