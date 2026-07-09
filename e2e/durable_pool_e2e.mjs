// Durable dispatch pool (O2/O4) end to end, over the real server binary.
//
// Proves the shared-queue + process-level pool path: a background-submitted run is
// driven autonomously by the pool (no foreground request drives it) over the ONE
// shared durable queue, its result is committed, the history survives a process
// restart (durable store), and the pool keeps draining after the restart.
//
// Run: node e2e/durable_pool_e2e.mjs

import { mkdtempSync } from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { spawnServer, stopServer, waitForPort, pass } from './harness.mjs';

const PORT = Number(process.env.E2E_PORT ?? 38790);
const THREAD = 'durable-pool-1';
const BASE = `http://127.0.0.1:${PORT}`;
// Durable ingress (which spawns the pool) over a shared SQLite dispatch queue.
const ENV = { AWAKEN_INGRESS: 'durable', AWAKEN_STORAGE_DIR: mkdtempSync(path.join(tmpdir(), 'awaken-durable-pool-')) };

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
// committed — the caller observes durable completion by polling, not by driving.
async function waitForAssistant(minCount, timeoutMs = 20_000) {
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
  // 1. First process: submit a background run and let the POOL drive it.
  let { server } = spawnServer('echo', PORT, ENV);
  try {
    await waitForPort(PORT);
    const runId = await submitBackground('hello pool');
    pass(`background run queued (${runId}) — no foreground request will drive it`);

    const { assistants } = await waitForAssistant(1);
    pass(`the pool drove the durable run to completion over the shared queue (reply: ${JSON.stringify(assistants[0].text)})`);
  } finally {
    await stopServer(server);
  }
  pass('first process stopped — the run and its result are now only in the durable store');

  // 2. Restart over the SAME store: committed history survives, and the pool keeps
  //    draining a fresh submission.
  ({ server } = spawnServer('echo', PORT, ENV));
  try {
    await waitForPort(PORT);
    const survived = await messages();
    if (!survived.some((m) => m.role === 'Assistant' && (m.text ?? '').length > 0)) {
      throw new Error(`durable history did not survive the restart: ${JSON.stringify(survived)}`);
    }
    pass('committed history survived the process restart (durable store)');

    // A second background run after the restart is driven by the pool too.
    await submitBackground('after restart');
    const { assistants } = await waitForAssistant(2);
    pass(`the pool drained a fresh run after the restart (${assistants.length} assistant replies committed)`);
  } finally {
    await stopServer(server);
  }

  console.log('\nDURABLE POOL E2E PASS: background runs driven autonomously by the process pool over one shared durable queue, surviving a restart.');
}

main().catch((err) => {
  console.error(`\nDURABLE POOL E2E FAIL: ${err.stack ?? err}`);
  process.exit(1);
});
