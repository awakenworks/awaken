// Durable cross-restart recovery across a real process restart, via the official
// Anthropic TS SDK. A mutating tool parks for approval; its waiting ticket +
// transcript commit to a per-thread SQLite database under AWAKEN_STORAGE_DIR. We
// KILL the server process and start a fresh one over the same storage directory,
// then approve on the SAME session. The rebuilt process has no in-memory session
// state, so the managed adapter rehydrates the session from committed truth
// (ADR-0039 lazy session rehydration) and the host recovers the parked run from
// the SQLite file — the run resumes and completes end-to-end. This exercises the
// durable commit + hydrate + fact-authority read path through HTTP.
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

  // Approve on the fresh process: the adapter rehydrates the session from durable
  // truth (ADR-0039) and the host recovers the parked run from the SQLite file, so
  // the run resumes and completes end-to-end — no in-memory session state needed.
  try {
    await client.beta.sessions.events.send(session.id, {
      events: [{ type: 'user.tool_confirmation', tool_use_id: toolUse.id, result: 'allow' }],
      betas: BETAS,
    });
    const events = await listEvents(session.id);
    const lastIdle = [...events].reverse().find((e) => e.type === 'session.status_idle');
    assert.equal(lastIdle.stop_reason.type, 'end_turn', 'parked run resumed and completed after restart');
    const results = events.filter((e) => e.type === 'agent.tool_result');
    assert.ok(
      JSON.stringify(results.at(-1)?.content ?? '').includes('SURVIVE-RESTART'),
      'read-back reflects the pre-restart write — durable state resumed on a fresh process',
    );
    pass('parked run resumed from durable truth on a fresh process and completed');
    console.log('E2E PASS: durable cross-restart recovery via TS SDK.');
  } finally {
    await stopServer(b.server);
    fs.rmSync(STORE_DIR, { recursive: true, force: true });
  }
}

main().catch((err) => {
  console.error('E2E FAIL:', err);
  process.exitCode = 1;
});
