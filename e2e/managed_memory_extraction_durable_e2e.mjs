// Cross-session EXTRACTION memory must be durable across a real process restart.
//
// `AWAKEN_MODEL_MODE=memory` runs the out-of-band extractor: a turn saves a memory
// that a LATER session recalls (extract -> store -> recall -> inject). That loop is
// already proven within one process by managed_memory_e2e.mjs. This test proves the
// missing half: the store must survive the process dying, governed by the SAME
// durable storage dir as every other piece of committed state (AWAKEN_STORAGE_DIR),
// NOT a separate opt-in var or a pid-namespaced temp dir.
//
// Flow: session A saves a memory; a later session in the SAME process recalls it
// (sanity). Then KILL the server and start a fresh one over the SAME
// AWAKEN_STORAGE_DIR. A new session must STILL recall the memory. If memory lives
// outside the durable storage dir it is lost on restart — the gap this test catches.
//
// Run: (from e2e/)  node managed_memory_extraction_durable_e2e.mjs

import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import { execFileSync } from 'node:child_process';
import Anthropic from '@anthropic-ai/sdk';
import { spawnServer, stopServer, waitForPort, pass, startUpstream, realServerEnv } from './harness.mjs';

const PORT = Number(process.env.E2E_PORT ?? 38213);
const BETAS = ['managed-agents-2026-04-01'];
const STORE_DIR = `/tmp/awaken-mem-extract-durable-e2e-${process.pid}`;
// A distinctive, once-only memory. The deterministic extractor saves a memory
// named after a `fact-<tag>` token in the transcript; a recall prompt WITHOUT such
// a token makes the extractor fall back to its fixed sky memory, so it never
// recreates this one. That is what makes the restart test honest: after a restart,
// the marker can only reappear if it was truly persisted, not re-extracted.
const MARKER = 'fact-zebra7durable';

let client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: `http://127.0.0.1:${PORT}` });

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

function extractionIntents(sessionId) {
  const database = path.join(STORE_DIR, 'sessions.db');
  if (!fs.existsSync(database)) return [];
  const output = execFileSync('sqlite3', [
    '-json',
    database,
    'SELECT data FROM managed_memory_extraction ORDER BY intent_id',
  ]).toString().trim();
  return (output ? JSON.parse(output) : [])
    .map((row) => JSON.parse(row.data))
    .filter((intent) => intent.session_id === sessionId);
}

async function reply(sessionId) {
  const events = [];
  for await (const ev of client.beta.sessions.events.list(sessionId, { betas: BETAS })) events.push(ev);
  return events
    .filter((e) => e.type === 'agent.message')
    .map((e) => e.content.map((b) => b.text ?? '').join(''))
    .join('\n');
}

async function turn(sessionId, text) {
  await client.beta.sessions.events.send(sessionId, {
    betas: BETAS,
    events: [{ type: 'user.message', content: [{ type: 'text', text }] }],
  });
  return reply(sessionId);
}

// Poll fresh sessions until the recall plugin injects the marker memory (the
// extractor is fire-and-forget). The recall prompt carries no `fact-` token, so it
// never re-extracts the marker — a positive can only come from the persisted store.
async function recallsMarker(storeId, tries = 24) {
  for (let i = 0; i < tries; i += 1) {
    await sleep(500);
    const b = await client.beta.sessions.create({
      agent: 'assistant',
      betas: BETAS,
      resources: [{ type: 'memory_store', memory_store_id: storeId, mount_path: '/memory' }],
    });
    if ((await turn(b.id, 'please recall what you know')).includes(MARKER)) return true;
  }
  return false;
}

async function main() {
  fs.rmSync(STORE_DIR, { recursive: true, force: true });
  fs.mkdirSync(STORE_DIR, { recursive: true });
  const servers = [];
  const upstream = await startUpstream('memory');
  try {
    // ---- server A: save a memory, confirm it recalls in-process ----
    const a = spawnServer('memory', PORT, { AWAKEN_STORAGE_DIR: STORE_DIR, ...realServerEnv('memory', upstream, { mode: 'memory' }) });
    servers.push(a.server);
    await waitForPort(PORT);

    const store = await client.post('/v1/memory_stores', { body: { name: 'durable-extraction' } });
    const readOnly = await client.beta.sessions.create({
      agent: 'assistant',
      betas: BETAS,
      resources: [{
        type: 'memory_store',
        memory_store_id: store.id,
        mount_path: '/memory',
        access: 'read_only',
      }],
    });
    const readOnlyMarker = 'fact-readonly-must-not-extract';
    await assert.rejects(
      () => turn(readOnly.id, `remember ${readOnlyMarker}`),
      /read-only mount .* requested but backend does not enforce read-only/u,
      'a backend without an enforced read-only capability must fail before execution',
    );
    assert.deepEqual(
      extractionIntents(readOnly.id),
      [],
      'a read-only binding must not enqueue durable extraction work',
    );
    assert.deepEqual(
      (await client.get(`/v1/memory_stores/${store.id}/memories`)).data,
      [],
      'a denied read-only activation must not mutate the bound MemoryStore',
    );
    pass('read-only Memory failed closed before execution and created no extraction outbox');

    const s = await client.beta.sessions.create({
      agent: 'assistant',
      betas: BETAS,
      resources: [{ type: 'memory_store', memory_store_id: store.id, mount_path: '/memory' }],
    });
    assert.ok((await turn(s.id, `remember ${MARKER}`)).includes(`echo:remember ${MARKER}`), 'turn A ran');
    assert.ok(await recallsMarker(store.id), 'a later session recalled the marker memory in-process (sanity)');
    pass('extraction memory saved and recalled within server A');

    // ---- restart: kill A, start B over the SAME storage dir ----
    await stopServer(a.server);
    servers.pop();
    const b = spawnServer('memory', PORT, { AWAKEN_STORAGE_DIR: STORE_DIR, ...realServerEnv('memory', upstream, { mode: 'memory' }) });
    servers.push(b.server);
    await waitForPort(PORT);
    client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: `http://127.0.0.1:${PORT}` });

    assert.ok(
      await recallsMarker(store.id),
      'a new session AFTER restart still recalls the extracted memory (durable under AWAKEN_STORAGE_DIR)',
    );
    pass('extraction memory survived a real process restart');
    console.log('E2E PASS: cross-session extraction memory is durable across restart.');
  } finally {
    for (const srv of servers) await stopServer(srv);
    upstream.close();
    fs.rmSync(STORE_DIR, { recursive: true, force: true });
  }
}

main().catch((err) => {
  console.error('E2E FAIL:', err);
  process.exitCode = 1;
});
