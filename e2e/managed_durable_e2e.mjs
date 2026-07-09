// Durable run-ingress end-to-end (slice D), via the official Anthropic TS SDK.
//
// With AWAKEN_INGRESS=durable the host delivers each turn through a
// `DurableRunIngress`: the accepted run is persisted to a per-thread SQLite
// dispatch queue, then driven by the dispatch worker (`submit_background`) — the
// same runtime and commit boundary a direct ingress uses (G6), only the delivery
// guarantee differs. So a normal echo turn here exercises the whole run-ingress
// path: enqueue → claim → lease → worker execute → commit relay.
//
// We then KILL the process and start a fresh one over the same storage directory.
// The rebuilt session rehydrates from committed truth and the durable ingress runs
// startup recovery against the surviving dispatch store, then a follow-up turn
// completes — proving the dispatch queue is durable and the ingress reconnects to
// it across a real restart.
//
// Run: (from e2e/)  node managed_durable_e2e.mjs

import assert from 'node:assert/strict';
import fs from 'node:fs';
import Anthropic from '@anthropic-ai/sdk';
import { spawnServer, stopServer, waitForPort, pass, startUpstream, realServerEnv } from './harness.mjs';

const PORT = Number(process.env.E2E_PORT ?? 38170);
const BETAS = ['managed-agents-2026-04-01'];
const STORE_DIR = `/tmp/awaken-durable-e2e-${process.pid}`;
const DURABLE_ENV = { AWAKEN_STORAGE_DIR: STORE_DIR, AWAKEN_INGRESS: 'durable' };

// `let`, not `const`: after the restart the old keep-alive socket is dead, so the
// post-restart calls use a freshly connected client.
let client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: `http://127.0.0.1:${PORT}` });

const listEvents = async (sessionId) => {
  const events = [];
  for await (const ev of client.beta.sessions.events.list(sessionId, { betas: BETAS })) events.push(ev);
  return events;
};

const assistantText = (events) =>
  JSON.stringify(events.filter((e) => e.type === 'agent.message').map((m) => m.content));

// The dispatch-queue databases (`*-dispatch.db`) — the run-ingress durable
// artifact, distinct from the commit store's per-thread `<thread>.db`.
// The durable dispatch queue is one process-shared file `dispatch.db` (the
// process-level DispatchPool over one shared queue), not the old per-thread
// `<thread>-dispatch.db` files.
const dispatchDbs = (dir) =>
  fs
    .readdirSync(dir, { withFileTypes: true })
    .filter((e) => e.isFile() && e.name.endsWith('dispatch.db'))
    .map((e) => e.name)
    .sort();

async function turn(sessionId, text) {
  await client.beta.sessions.events.send(sessionId, {
    events: [{ type: 'user.message', content: [{ type: 'text', text }] }],
    betas: BETAS,
  });
  return listEvents(sessionId);
}

async function main() {
  fs.rmSync(STORE_DIR, { recursive: true, force: true });
  fs.mkdirSync(STORE_DIR, { recursive: true });

  // One fake upstream survives the restart, so both processes dial the same wire.
  const upstream = await startUpstream('echo');
  const realEnv = { ...DURABLE_ENV, ...realServerEnv('echo', upstream) };

  // ---- server A: a turn delivered through the durable dispatch queue ----
  const a = spawnServer('real', PORT, realEnv);
  await waitForPort(PORT);

  const session = await client.beta.sessions.create({
    agent: 'assistant',
    environment_id: 'env_local',
    betas: BETAS,
  });
  const first = await turn(session.id, 'DURABLE-ONE');
  assert.ok(assistantText(first).includes('DURABLE-ONE'), 'turn ran through the durable ingress and echoed');
  const idle = [...first].reverse().find((e) => e.type === 'session.status_idle');
  assert.equal(idle.stop_reason.type, 'end_turn', 'durable turn drove to a terminal phase');
  pass('turn delivered through submit_background → dispatch worker → commit');

  const dbsBefore = dispatchDbs(STORE_DIR);
  assert.ok(dbsBefore.length >= 1, 'the accepted run persisted to a durable dispatch queue on disk');
  pass(`dispatch queue on disk: ${dbsBefore.join(', ')}`);

  // ---- kill A, start a fresh server B over the SAME storage directory ----
  await stopServer(a.server);
  const b = spawnServer('real', PORT, realEnv);
  await waitForPort(PORT);
  client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: `http://127.0.0.1:${PORT}` });

  assert.deepEqual(dispatchDbs(STORE_DIR), dbsBefore, 'the durable dispatch queue survived the restart');
  pass('dispatch queue persisted on disk across a real process restart');

  try {
    // A follow-up turn on the SAME session: the rebuilt process has no in-memory
    // session state, so it rehydrates from committed truth and the durable ingress
    // runs startup recovery against the surviving dispatch store before driving the
    // new run. Prior history is present, proving cross-restart continuity.
    const second = await turn(session.id, 'DURABLE-TWO');
    const replies = assistantText(second);
    assert.ok(replies.includes('DURABLE-TWO'), 'follow-up turn ran through the durable ingress on the fresh process');
    assert.ok(replies.includes('DURABLE-ONE'), 'pre-restart turn survived in committed truth (multi-turn continuity)');
    pass('durable ingress reconnected to the persisted queue and continued the thread after restart');

    console.log('E2E PASS: durable run-ingress dispatch + cross-restart recovery via TS SDK.');
  } finally {
    await stopServer(b.server);
    upstream.close();
    fs.rmSync(STORE_DIR, { recursive: true, force: true });
  }
}

main().catch((err) => {
  console.error('E2E FAIL:', err);
  process.exitCode = 1;
});
