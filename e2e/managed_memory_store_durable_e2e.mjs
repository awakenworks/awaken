// Memory_store RESOURCE durability across a real process restart (ADR-0038).
//
// The ADR-0038 MemoryStore family gives a session a stable, mutable id it mounts
// read-write: the agent edits the realized file and the host harvests the write
// back under the same id, so a memory written in one session is visible to the
// next. This test proves that write-back is *durable* — it must survive the server
// process dying, not just live in one process's heap.
//
// Flow: create a memory store, run a session whose deterministic model writes a
// marker into the mount (harvested on turn end), assert `GET /v1/memory_stores/:id`
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
import { spawnServer, stopServer, waitForPort, pass } from './harness.mjs';

const PORT = Number(process.env.E2E_PORT ?? 38211);
const BETAS = ['managed-agents-2026-04-01'];
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
// confirmation, so releasing it lets the harvest run.
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
  try {
    const cur = await client.get(`/v1/memory_stores/${id}`);
    return cur?.content ?? '';
  } catch {
    return '';
  }
}

// `GET /v1/files?scope_id=<session>` is the reverse-channel trigger: it harvests a
// session's read-write memory mounts back into their stores before listing output
// artifacts. Poking it forces the write-back to run.
async function harvest(sid) {
  try {
    await client.get(`/v1/files?scope_id=${sid}`);
  } catch {
    /* ignore */
  }
}

async function main() {
  fs.rmSync(STORE_DIR, { recursive: true, force: true });
  fs.mkdirSync(STORE_DIR, { recursive: true });
  const servers = [];
  try {
    // ---- server A: write into a mounted memory store; host harvests it ----
    const a = spawnServer('memory-resource', PORT, { AWAKEN_STORAGE_DIR: STORE_DIR });
    servers.push(a.server);
    await waitForPort(PORT);

    const mem = await client.post('/v1/memory_stores');
    assert.ok(mem.id, 'POST /v1/memory_stores returned an id');

    const session = await client.beta.sessions.create({
      agent: 'assistant',
      environment_id: 'env_local',
      resources: [{ type: 'memory_store', memory_store_id: mem.id, mount_path: '/notes.txt' }],
      betas: BETAS,
    });
    await client.beta.sessions.events.send(session.id, {
      events: [{ type: 'user.message', content: [{ type: 'text', text: MARKER }] }],
      betas: BETAS,
    });

    // Drive the write -> approve -> harvest loop until the store reflects the marker.
    const approved = new Set();
    let harvested = '';
    for (let i = 0; i < 40; i += 1) {
      await sleep(400);
      await approveGated(session.id, await listEvents(session.id), approved);
      await harvest(session.id);
      harvested = await memContent(mem.id);
      if (harvested.includes(MARKER)) break;
    }
    assert.ok(
      harvested.includes(MARKER),
      `server A harvested the write into the memory store: ${JSON.stringify(harvested)}`,
    );
    pass('write -> harvest landed the marker in the memory store (pre-restart)');

    // ---- restart: kill A, start B over the SAME storage dir ----
    await stopServer(a.server);
    servers.pop();
    const b = spawnServer('memory-resource', PORT, { AWAKEN_STORAGE_DIR: STORE_DIR });
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
    fs.rmSync(STORE_DIR, { recursive: true, force: true });
  }
}

main().catch((err) => {
  console.error('E2E FAIL:', err);
  process.exitCode = 1;
});
