// Durability boundary across a real process restart, via the official Anthropic
// TS SDK. This pins down exactly how far durability reaches today:
//
//   * Runtime truth IS durable. A mutating tool parks for approval; the waiting
//     ticket + transcript commit to a per-thread SQLite database under
//     AWAKEN_STORAGE_DIR, and that database survives the server process dying.
//   * The managed *session registry* is NOT durable. `ManagedState.sessions` is
//     an in-memory map, so a fresh process does not know the old `session.id` and
//     returns 404 — even though the run's committed truth is on disk.
//
// So the missing piece for end-to-end resume is a durable session→thread
// registry (or lazy session rehydration from committed truth), not the store.
// When that lands, flip the post-restart expectation from 404 to a completed
// resume. Until then this guards the boundary and exercises the durable commit
// path through HTTP.
//
// Run: (from e2e/)  node managed_restart_e2e.mjs

import assert from 'node:assert/strict';
import fs from 'node:fs';
import Anthropic from '@anthropic-ai/sdk';
import { spawnServer, stopServer, waitForPort, pass } from './harness.mjs';

const PORT = Number(process.env.E2E_PORT ?? 38130);
const BETAS = ['managed-agents-2026-04-01'];
const STORE_DIR = `/tmp/awaken-restart-e2e-${process.pid}`;

const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: `http://127.0.0.1:${PORT}` });

const listEvents = async (sessionId) => {
  const events = [];
  for await (const ev of client.beta.sessions.events.list(sessionId, { betas: BETAS })) events.push(ev);
  return events;
};

async function main() {
  fs.rmSync(STORE_DIR, { recursive: true, force: true });
  fs.mkdirSync(STORE_DIR, { recursive: true });

  // ---- server A: start a run that parks on a tool confirmation ----
  const a = spawnServer('probe', PORT, { AWAKEN_STORAGE_DIR: STORE_DIR });
  await waitForPort(PORT);

  const session = await client.beta.sessions.create({
    agent: 'assistant',
    environment_id: 'env_local',
    betas: BETAS,
  });
  await client.beta.sessions.events.send(session.id, {
    events: [{ type: 'user.message', content: [{ type: 'text', text: 'SURVIVE-RESTART' }] }],
    betas: BETAS,
  });

  const events = await listEvents(session.id);
  const toolUse = events.find((e) => e.type === 'agent.tool_use');
  assert.ok(toolUse, 'run parked on a tool_use before restart');
  assert.equal(
    events.find((e) => e.type === 'session.status_idle').stop_reason.type,
    'requires_action',
    'parked awaiting confirmation',
  );
  const dbsBefore = fs.readdirSync(STORE_DIR).filter((f) => f.endsWith('.db'));
  assert.ok(dbsBefore.length >= 1, 'the parked run committed to a per-thread sqlite database');
  pass(`run parked; durable db on disk: ${dbsBefore.join(', ')}`);

  // ---- kill A, start a fresh server B over the SAME storage directory ----
  await stopServer(a.server);
  const b = spawnServer('probe', PORT, { AWAKEN_STORAGE_DIR: STORE_DIR });
  await waitForPort(PORT);

  // The durable truth survived the process death (this is the guarantee the
  // store layer provides — ADR-0039 D4 / ADR-0006).
  const dbsAfter = fs.readdirSync(STORE_DIR).filter((f) => f.endsWith('.db'));
  assert.deepEqual(dbsAfter, dbsBefore, 'the committed sqlite database survived the restart');
  pass('committed truth persisted on disk across a real process restart');

  // The managed session registry is in-memory, so the fresh process cannot resolve
  // the old session id yet. This documents the boundary; flip to a resume when a
  // durable session registry lands.
  try {
    await client.beta.sessions.events.send(session.id, {
      events: [{ type: 'user.tool_confirmation', tool_use_id: toolUse.id, result: 'allow' }],
      betas: BETAS,
    });
    assert.fail('expected 404: managed sessions are not durable yet');
  } catch (err) {
    assert.equal(err.status, 404, 'managed session is not recoverable after restart (in-memory registry)');
    pass('boundary confirmed: store is durable, managed session registry is not (needs durable sessions)');
  } finally {
    await stopServer(b.server);
    fs.rmSync(STORE_DIR, { recursive: true, force: true });
  }

  console.log('E2E PASS: durability boundary (durable store, non-durable managed session) verified.');
}

main().catch((err) => {
  console.error('E2E FAIL:', err);
  process.exitCode = 1;
});
