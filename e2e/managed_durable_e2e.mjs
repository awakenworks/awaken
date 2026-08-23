// Durable run-ingress end-to-end (slice D), via the official Anthropic TS SDK.
//
// With SESSION_DEPLOYMENT_INGRESS=durable the host delivers each Run through a
// `DurableRunIngress`: the accepted run is persisted to a per-thread SQLite
// dispatch queue, then driven by the dispatch worker (`submit_background`) — the
// same runtime and commit boundary a direct ingress uses (G6), only the delivery
// guarantee differs. So a normal echo Run here exercises the whole run-ingress
// path: enqueue → claim → lease → worker execute → commit relay.
//
// We then KILL the process and start a fresh one over the same storage directory.
// The rebuilt session rehydrates from committed truth and the durable ingress runs
// startup recovery against the surviving dispatch store, then a follow-up Run
// completes — proving the dispatch queue is durable and the ingress reconnects to
// it across a real restart.
//
// Run: (from e2e/)  node managed_durable_e2e.mjs

import assert from 'node:assert/strict';
import fs from 'node:fs';
import Anthropic from '@anthropic-ai/sdk';
import {
  pass,
  realServerEnv,
  spawnServer,
  startUpstream,
  stopServer,
  waitForPort,
  waitForSessionEventReceipt,
} from './harness.mjs';

const PORT = Number(process.env.E2E_PORT ?? 38170);
const BETAS = ['managed-agents-2026-04-01'];
const STORE_DIR = `/tmp/awaken-durable-e2e-${process.pid}`;
const DURABLE_ENV = { SESSION_DEPLOYMENT_STORAGE_DIR: STORE_DIR, SESSION_DEPLOYMENT_INGRESS: 'durable' };

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

async function sendRun(sessionId, text) {
  const receipt = await client.beta.sessions.events.send(sessionId, {
    events: [{ type: 'user.message', content: [{ type: 'text', text }] }],
    betas: BETAS,
  });
  const acceptedId = receipt.data[0]?.id;
  assert.equal(typeof acceptedId, 'string', 'durable Run returns its exact User Event receipt');
  const { events } = await waitForSessionEventReceipt(
    client,
    sessionId,
    acceptedId,
    BETAS,
    ({ delta }) => delta.some((event) => event.type === 'agent.message')
      && delta.some((event) => event.type === 'session.status_idle'),
    `durable Run ${text} to commit its reply and terminal Session status`,
    { timeoutMs: 30_000 },
  );
  return events;
}

async function main() {
  // Cause/effect graph: C1=durable ingress returns an initially unprocessed User
  // Event receipt; C2=the dispatch
  // worker claims and commits it; C3=a fresh process opens the same storage;
  // C4=a follow-up Run targets the same Session. Effects: E1=one on-disk queue
  // owns delivery; E2=the first reply settles terminal; E3=the queue survives;
  // E4=rehydration preserves prior history and commits the follow-up. Decision
  // rules: R1 C1 && C2 => E1-E2; R2 R1 && C3 => E3; R3 R2 && C4 => E4.
  // Decision rule summary: admission+claim proves delivery; restart+follow-up
  // proves durable rehydration through the same queue and Session identity.
  // Constraints/invariant: one persisted dispatch queue owns delivery and the
  // replacement process reuses, rather than recreates, Session/Run truth.
  fs.rmSync(STORE_DIR, { recursive: true, force: true });
  fs.mkdirSync(STORE_DIR, { recursive: true });

  // One fake upstream survives the restart, so both processes dial the same wire.
  const upstream = await startUpstream('echo');
  const realEnv = { ...DURABLE_ENV, ...realServerEnv('echo', upstream) };

  // ---- server A: a Run delivered through the durable dispatch queue ----
  const a = spawnServer('real', PORT, realEnv);
  // An instrumented all-suite build can spend several minutes in loader and
  // migration work before accepting connections. Tie readiness to the child so
  // an actual early exit still fails immediately, while slow coverage startup
  // is not misclassified as a dead server.
  await waitForPort(PORT, 900_000, a.server);

  const session = await client.beta.sessions.create({
    agent: 'assistant',
    environment_id: 'env_local',
    betas: BETAS,
  });
  const first = await sendRun(session.id, 'DURABLE-ONE');
  assert.ok(assistantText(first).includes('DURABLE-ONE'), 'Run passed through the durable ingress and echoed');
  const idle = [...first].reverse().find((e) => e.type === 'session.status_idle');
  assert.equal(idle.stop_reason.type, 'end_turn', 'durable Run reached a terminal phase');
  pass('Run delivered through submit_background → dispatch worker → commit');

  const dbsBefore = dispatchDbs(STORE_DIR);
  assert.ok(dbsBefore.length >= 1, 'the accepted run persisted to a durable dispatch queue on disk');
  pass(`dispatch queue on disk: ${dbsBefore.join(', ')}`);

  // ---- kill A, start a fresh server B over the SAME storage directory ----
  await stopServer(a.server);
  const b = spawnServer('real', PORT, realEnv);
  await waitForPort(PORT, 900_000, b.server);
  client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: `http://127.0.0.1:${PORT}` });

  assert.deepEqual(dispatchDbs(STORE_DIR), dbsBefore, 'the durable dispatch queue survived the restart');
  pass('dispatch queue persisted on disk across a real process restart');

  try {
    // A follow-up Run on the SAME session: the rebuilt process has no in-memory
    // session state, so it rehydrates from committed truth and the durable ingress
    // runs startup recovery against the surviving dispatch store before driving the
    // new run. Prior history is present, proving cross-restart continuity.
    const second = await sendRun(session.id, 'DURABLE-TWO');
    const replies = assistantText(second);
    assert.ok(replies.includes('DURABLE-TWO'), 'follow-up Run passed through the durable ingress on the fresh process');
    assert.ok(replies.includes('DURABLE-ONE'), 'pre-restart Run survived in committed truth (multi-Run continuity)');
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
