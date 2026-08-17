// Memory_store RESOURCE durability across a real process restart (ADR-0038).
//
// The ADR-0038 MemoryStore family gives a session a stable, mutable id it mounts
// read-write: the agent edits the realized file and the MemoryMount guard reconciles
// the copy at Session release under the same id, so a memory written in one session is visible to the
// next. This test proves that write-back is *durable* — it must survive the server
// process dying, not just live in one process's heap.
//
// Flow: create a memory store, run a session whose deterministic model writes a
// marker into the mount, release the Session, assert the `/memories` content
// reflects it, then KILL the server and start a fresh one over the SAME storage
// dir. The marker must still be there. A purely in-memory store loses it on
// restart — that is the completeness gap this test exists to catch.
//
// Deterministic (`memory-resource` mode) so it runs in CI without an API key.
//
// Run: (from e2e/)  node managed_memory_store_durable_e2e.mjs

import assert from 'node:assert/strict';
import fs from 'node:fs';
import Anthropic from '@anthropic-ai/sdk';
import {
  cleanupFixtureTree,
  hasEndTurn,
  pass,
  realServerEnv,
  spawnServer,
  startUpstream,
  stopServer,
  waitForPort,
} from './harness.mjs';

const PORT = Number(process.env.E2E_PORT ?? 38211);
const BETAS = ['managed-agents-2026-04-01'];
const MEMORY_HEADERS = { 'anthropic-beta': 'agent-memory-2026-07-22' };
const STORE_DIR = `/tmp/awaken-memstore-durable-e2e-${process.pid}`;
const MARKER = 'PERSISTED_MARKER_7788';

let client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: `http://127.0.0.1:${PORT}` });

const listEvents = async (sid) => {
  const evs = [];
  for await (const ev of client.beta.sessions.events.list(sid, { betas: BETAS })) evs.push(ev);
  return evs;
};

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

// Approve every gated (`ask`) tool call not yet approved — `write` parks for a
// confirmation, so releasing it lets the turn complete before Session release.
async function approveGated(sid, evs, approved) {
  for (const e of evs) {
    if (e.type === 'agent.tool_use' && e.evaluated_permission === 'ask' && !approved.has(e.id)) {
      approved.add(e.id);
      await client.beta.sessions.events.send(sid, {
        events: [{ type: 'user.tool_confirmation', tool_use_id: e.id, result: 'allow' }],
        betas: BETAS,
      });
    }
  }
}

async function memContent(id) {
  // Observation rule O1: a successful list projects the durable contents;
  // O2: any HTTP/decode failure must fail the test at its real cause. Returning
  // an empty string for O2 would falsely classify transport failure as an empty
  // MemoryStore and hide the failing boundary.
  // Decision rule O3: content assertion -> request `view=full`; the canonical
  // default `basic` view intentionally returns `content: null` and therefore
  // cannot distinguish an existing Memory from an empty store.
  const page = await client.get(`/v1/memory_stores/${id}/memories?view=full`, {
    headers: MEMORY_HEADERS,
  });
  return (page?.data ?? []).map((memory) => memory.content ?? '').join('\n');
}

async function main() {
  cleanupFixtureTree(STORE_DIR);
  fs.mkdirSync(STORE_DIR, { recursive: true });
  const servers = [];
  const upstream = await startUpstream('memoryResource');
  try {
    // ---- server A: write into a mounted memory store; release reconciles it ----
    const a = spawnServer('memory-resource', PORT, { SESSION_DEPLOYMENT_STORAGE_DIR: STORE_DIR, ...realServerEnv('memoryResource', upstream, { mode: 'memory-resource' }) });
    servers.push(a.server);
    await waitForPort(PORT);

    const mem = await client.post('/v1/memory_stores', {
      body: { name: 'durable-memory-store' },
      headers: MEMORY_HEADERS,
    });
    assert.ok(mem.id, 'POST /v1/memory_stores returned an id');

    const session = await client.beta.sessions.create({
      agent: 'assistant',
      environment_id: 'env_local',
      resources: [{ type: 'memory_store', memory_store_id: mem.id, mount_path: '/memory' }],
      betas: BETAS,
    });
    await client.beta.sessions.events.send(session.id, {
      events: [{ type: 'user.message', content: [{ type: 'text', text: MARKER }] }],
      betas: BETAS,
    });

    // Drive write -> approval until the turn completes, then release the mount once.
    const approved = new Set();
    let completed = false;
    for (let i = 0; i < 40; i += 1) {
      await sleep(400);
      const events = await listEvents(session.id);
      await approveGated(session.id, events, approved);
      completed = hasEndTurn(events);
      if (completed) break;
    }
    assert.ok(completed, 'the memory-writing turn completed');
    const completedEvents = await listEvents(session.id);
    const toolResults = completedEvents.filter((event) => event.type === 'agent.tool_result');
    const writeResult = toolResults[0];
    assert.ok(writeResult, 'the gated write produced a tool result before release');
    assert.notEqual(
      writeResult.is_error,
      true,
      `the Memory mount write succeeded: ${JSON.stringify(writeResult.content)}`,
    );
    assert.ok(
      JSON.stringify(toolResults.at(-1)?.content).includes(MARKER),
      `the follow-up read observes the exact mounted bytes: ${JSON.stringify(toolResults)}`,
    );
    await client.beta.sessions.delete(session.id, { betas: BETAS });
    let harvested = '';
    for (let i = 0; i < 20; i += 1) {
      harvested = await memContent(mem.id);
      if (harvested.includes(MARKER)) break;
      await sleep(200);
    }
    assert.ok(
      harvested.includes(MARKER),
      `server A harvested the write into the memory store: ${JSON.stringify(harvested)}`,
    );
    pass('Session release reconciled the Memory mount into the store (pre-restart)');

    // ---- restart: kill A, start B over the SAME storage dir ----
    await stopServer(a.server);
    servers.pop();
    const b = spawnServer('memory-resource', PORT, { SESSION_DEPLOYMENT_STORAGE_DIR: STORE_DIR, ...realServerEnv('memoryResource', upstream, { mode: 'memory-resource' }) });
    servers.push(b.server);
    await waitForPort(PORT);
    client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: `http://127.0.0.1:${PORT}` });

    const after = await memContent(mem.id);
    assert.ok(
      after.includes(MARKER),
      `the memory store's contents survived the restart (got: ${JSON.stringify(after)})`,
    );
    pass('memory_store contents survived a real process restart');
    console.log('E2E PASS: ADR-0038 memory_store resource is durable across restart.');
  } finally {
    for (const s of servers) await stopServer(s);
    upstream.close();
    // Cleanup cause/effect rules: C1 no retained mount -> recursively remove;
    // C2 live or disconnected FUSE projection -> detach deepest-first, then
    // remove; C3 detach failure -> fail the test. FMECA: raw rmSync maps C2 to
    // EISDIR/ENOTCONN and leaks the fixture after the logically asynchronous
    // Session teardown; the canonical harness is the sole mount-aware owner.
    cleanupFixtureTree(STORE_DIR);
  }
}

main().catch((err) => {
  console.error('E2E FAIL:', err);
  process.exitCode = 1;
});
