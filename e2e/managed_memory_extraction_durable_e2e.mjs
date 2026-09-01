// Cross-session EXTRACTION memory must be durable across a real process restart.
//
// `AWAKEN_MODEL_MODE=memory` runs the out-of-band extractor: a turn saves a memory
// that a LATER session recalls (extract -> store -> recall -> inject). That loop is
// already proven within one process by managed_memory_e2e.mjs. This test proves the
// missing half: the store must survive the process dying, governed by the SAME
// durable storage dir as every other piece of committed state (SESSION_DEPLOYMENT_STORAGE_DIR),
// NOT a separate opt-in var or a pid-namespaced temp dir.
//
// Flow: session A saves a memory; a later session in the SAME process recalls it
// (sanity). Then KILL the server and start a fresh one over the SAME
// SESSION_DEPLOYMENT_STORAGE_DIR. A new session must STILL recall the memory. If memory lives
// outside the durable storage dir it is lost on restart — the gap this test catches.
//
// Run: (from e2e/)  node managed_memory_extraction_durable_e2e.mjs

import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { DatabaseSync } from 'node:sqlite';
import Anthropic from '@anthropic-ai/sdk';
import {
  assertPendingReceiptHasNoRuntimeEffects,
  cleanupFixtureTree,
  spawnServer,
  stopServer,
  waitForPort,
  pass,
  startUpstream,
  realServerEnv,
  scenarioMemoryStore,
  waitForSessionEventReceipt,
} from './harness.mjs';

const PORT = Number(process.env.E2E_PORT ?? 38213);
const BETAS = ['managed-agents-2026-04-01'];
const MEMORY_HEADERS = { 'anthropic-beta': 'agent-memory-2026-07-22' };
const STORE_DIR = path.join(os.tmpdir(), `awaken-mem-extract-durable-e2e-${process.pid}`);
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
  const connection = new DatabaseSync(database, { readOnly: true });
  let rows;
  try {
    rows = connection
      .prepare('SELECT data FROM managed_memory_extraction ORDER BY intent_id')
      .all();
  } finally {
    connection.close();
  }
  return rows
    .map((row) => JSON.parse(row.data))
    .filter((intent) => intent.session_id === sessionId);
}

async function turn(sessionId, text) {
  // C1=exact User receipt; C2=memory reply+terminal. E1=post-C1 reply proves
  // this process incarnation. K: extraction intent/storage has its own durable
  // oracle. Decision M1 C1&&!C2=>retry; M2 C1+C2=>return committed reply.
  const receipt = await client.beta.sessions.events.send(sessionId, {
    betas: BETAS,
    events: [{ type: 'user.message', content: [{ type: 'text', text }] }],
  });
  const receiptId = receipt.data[0]?.id;
  assert.equal(typeof receiptId, 'string', 'M1 exact durable-memory User Event receipt');
  const { delta } = await waitForSessionEventReceipt(
    client,
    sessionId,
    receiptId,
    BETAS,
    ({ delta: later }) => later.some((event) => event.type === 'agent.message')
      && later.some((event) => event.type === 'session.status_idle'),
    `M1 durable-memory Run for ${JSON.stringify(text)} to commit`,
  );
  return delta
    .filter((event) => event.type === 'agent.message')
    .map((event) => event.content.map((block) => block.text ?? '').join(''))
    .join('\n');
}

// Poll fresh sessions until the recall plugin injects the marker memory (the
// extractor is fire-and-forget). The recall prompt carries no `fact-` token, so it
// never re-extracts the marker — a positive can only come from the persisted store.
async function recallsMarker(storeId, tries = 24) {
  for (let i = 0; i < tries; i += 1) {
    await sleep(500);
    const b = await client.beta.sessions.create({
      agent: 'assistant',
      environment_id: 'env_local',
      betas: BETAS,
      resources: [{ type: 'memory_store', memory_store_id: storeId }],
    });
    if ((await turn(b.id, 'please recall what you know')).includes(MARKER)) return true;
  }
  return false;
}

async function main() {
  cleanupFixtureTree(STORE_DIR);
  fs.mkdirSync(STORE_DIR, { recursive: true });
  const servers = [];
  const upstream = await startUpstream('memory');
  try {
    // ---- server A: save a memory, confirm it recalls in-process ----
    const a = spawnServer('memory', PORT, { SESSION_DEPLOYMENT_STORAGE_DIR: STORE_DIR, ...realServerEnv('memory', upstream, { mode: 'memory' }) });
    servers.push(a.server);
    await waitForPort(PORT);

    const store = await scenarioMemoryStore(client, MEMORY_HEADERS);
    const readOnly = await client.beta.sessions.create({
      agent: 'assistant',
      environment_id: 'env_local',
      betas: BETAS,
      resources: [{
        type: 'memory_store',
        memory_store_id: store.id,
        access: 'read_only',
      }],
    });
    const readOnlyMarker = 'fact-readonly-must-not-extract';
    const modelRequestsBeforeReadOnly = upstream.requests.length;
    // Read-only capability decision table: C1=the User batch is durably
    // admitted; C2=the local backend cannot enforce the frozen read-only mount;
    // C3=no committed projection anchor exists; C4=one bounded reconciliation
    // window elapses. E1=return the exact unprocessed receipt from admission;
    // E2=expose that receipt exactly once as pending list history; E3=keep
    // the Session idle/nonterminal; E4=publish no model/tool/terminal effect;
    // E5=enqueue no extraction or store mutation. K: the Session root retains
    // retryable command provenance and events.list projects its pending suffix;
    // this fixture must not invent a second pending-event surface.
    // Decision RO1 C1+C2=>E1; RO2 C1+C2+C3+C4=>E2+E3+E4+E5.
    const readOnlyReceipt = await client.beta.sessions.events.send(readOnly.id, {
      betas: BETAS,
      events: [{
        type: 'user.message',
        content: [{ type: 'text', text: `remember ${readOnlyMarker}` }],
      }],
    });
    const acceptedReadOnly = readOnlyReceipt.data[0];
    assert.equal(acceptedReadOnly?.type, 'user.message', 'RO1 exact User Event receipt family');
    assert.equal(
      acceptedReadOnly?.processed_at,
      null,
      'RO1 capability failure is not falsely processed',
    );
    await sleep(750);
    const readOnlyEvents = [];
    for await (const event of client.beta.sessions.events.list(readOnly.id, { betas: BETAS })) {
      readOnlyEvents.push(event);
    }
    assertPendingReceiptHasNoRuntimeEffects({
      history: readOnlyEvents,
      receiptId: acceptedReadOnly.id,
      forbiddenEventTypes: new Set([
        'agent.message',
        'agent.mcp_tool_use',
        'agent.mcp_tool_result',
        'agent.tool_use',
        'agent.tool_result',
        'session.error',
        'session.status_idle',
        'session.thread_status_idle',
        'session.usage',
        'span.model_request_start',
        'span.model_request_end',
      ]),
      description: 'RO2 denied read-only extraction',
    });
    assert.equal(
      (await client.beta.sessions.retrieve(readOnly.id, { betas: BETAS })).status,
      'idle',
      'RO2 capability failure remains idle and nonterminal',
    );
    assert.equal(upstream.requests.length, modelRequestsBeforeReadOnly, 'RO2 no Provider request');
    assert.deepEqual(
      extractionIntents(readOnly.id),
      [],
      'a read-only binding must not enqueue durable extraction work',
    );
    assert.deepEqual(
      (await client.get(`/v1/memory_stores/${store.id}/memories`, {
        headers: MEMORY_HEADERS,
      })).data,
      [],
      'a denied read-only activation must not mutate the bound MemoryStore',
    );
    pass('read-only Memory failed closed before execution and created no extraction outbox');

    const s = await client.beta.sessions.create({
      agent: 'assistant',
      environment_id: 'env_local',
      betas: BETAS,
      resources: [{ type: 'memory_store', memory_store_id: store.id }],
    });
    assert.ok((await turn(s.id, `remember ${MARKER}`)).includes(`echo:remember ${MARKER}`), 'turn A ran');
    assert.ok(await recallsMarker(store.id), 'a later session recalled the marker memory in-process (sanity)');
    pass('extraction memory saved and recalled within server A');

    // ---- restart: kill A, start B over the SAME storage dir ----
    await stopServer(a.server);
    servers.pop();
    const b = spawnServer('memory', PORT, { SESSION_DEPLOYMENT_STORAGE_DIR: STORE_DIR, ...realServerEnv('memory', upstream, { mode: 'memory' }) });
    servers.push(b.server);
    await waitForPort(PORT);
    client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: `http://127.0.0.1:${PORT}` });

    assert.ok(
      await recallsMarker(store.id),
      'a new session AFTER restart still recalls the extracted memory (durable under SESSION_DEPLOYMENT_STORAGE_DIR)',
    );
    pass('extraction memory survived a real process restart');
    console.log('E2E PASS: cross-session extraction memory is durable across restart.');
  } finally {
    for (const srv of servers) await stopServer(srv);
    upstream.close();
    // Crash/restart can leave a disconnected FUSE projection after the child
    // exits. The canonical fixture cleanup detaches every nested mount before
    // removing the tree; raw rmSync is not a valid cleanup oracle for EISDIR.
    cleanupFixtureTree(STORE_DIR);
  }
}

main().catch((err) => {
  console.error('E2E FAIL:', err);
  process.exitCode = 1;
});
