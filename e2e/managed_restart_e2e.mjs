// Durable cross-restart recovery across a real process restart, via the official
// Anthropic TS SDK. A mutating tool awaits for approval; its waiting ticket +
// transcript commit to a per-thread durable store under SESSION_DEPLOYMENT_STORAGE_DIR (SQLite
// by default, the filesystem append-log with SESSION_DEPLOYMENT_STORE=fs). We KILL the server
// process and start a fresh one over the same storage directory, then approve on
// the SAME session with a freshly connected client. The rebuilt process has no
// in-memory session state, so the managed adapter rehydrates the session from
// committed truth (ADR-0039 lazy session rehydration) and the host recovers the
// awaiting run from the store — the run resumes and completes end-to-end. This
// exercises the durable commit + hydrate + fact-authority read path through HTTP.
//
// Run: (from e2e/)  node managed_restart_e2e.mjs   (add SESSION_DEPLOYMENT_STORE=fs for fs)

import assert from 'node:assert/strict';
import fs from 'node:fs';
import Anthropic from '@anthropic-ai/sdk';
import {
  managedAgentWithAlwaysAskTools,
  spawnServer,
  stopServer,
  waitForPort,
  pass,
  startUpstream,
  realServerEnv,
  waitForSessionEventReceipt,
} from './harness.mjs';

const PORT = Number(process.env.E2E_PORT ?? 38130);
const BETAS = ['managed-agents-2026-04-01'];
const STORE_DIR = `/tmp/awaken-restart-e2e-${process.pid}`;

// `let`, not `const`: after the server restart the old keep-alive socket is dead,
// so the post-restart calls use a freshly connected client (see below).
let client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: `http://127.0.0.1:${PORT}` });

// The durable artifacts under the store dir, regardless of backend: SQLite `.db`
// files or the filesystem backend's `commits.ndjson` append-logs.
const durableFiles = (dir) => {
  const out = [];
  for (const entry of fs.readdirSync(dir, { withFileTypes: true })) {
    const p = `${dir}/${entry.name}`;
    if (entry.isDirectory()) out.push(...durableFiles(p));
    else if (entry.name.endsWith('.db') || entry.name.endsWith('.ndjson')) out.push(p);
  }
  return out.sort();
};

async function main() {
  fs.rmSync(STORE_DIR, { recursive: true, force: true });
  fs.mkdirSync(STORE_DIR, { recursive: true });

  // ---- server A: start a run that awaits on a tool confirmation ----
  const upstream = await startUpstream('probe');
  let activeServer = null;
  try {
    const a = spawnServer('real', PORT, { SESSION_DEPLOYMENT_STORAGE_DIR: STORE_DIR, ...realServerEnv('probe', upstream) });
    activeServer = a.server;
    await waitForPort(PORT);

    const session = await client.beta.sessions.create({
      agent: managedAgentWithAlwaysAskTools(['write']),
      environment_id: 'env_local',
      betas: BETAS,
    });
    const initialReceipt = await client.beta.sessions.events.send(session.id, {
      events: [{ type: 'user.message', content: [{ type: 'text', text: 'SURVIVE-RESTART' }] }],
      betas: BETAS,
    });

    const initialReceiptId = initialReceipt.data[0]?.id;
    assert.equal(typeof initialReceiptId, 'string', 'R1 exact pre-restart User Event receipt');
    const { events: initialEvents } = await waitForSessionEventReceipt(
      client,
      session.id,
      initialReceiptId,
      BETAS,
      ({ delta }) => delta.some((event) => event.type === 'agent.tool_use')
        && [...delta].reverse().find((event) => event.type === 'session.status_idle')?.stop_reason?.type === 'requires_action',
      'R1 pre-restart Run to commit its awaiting state',
    );
    const toolUse = initialEvents.find((e) => e.type === 'agent.tool_use');
    assert.ok(toolUse, 'run awaiting on a tool_use before restart');
    assert.equal(
      initialEvents.find((e) => e.type === 'session.status_idle').stop_reason.type,
      'requires_action',
      'awaiting awaiting confirmation',
    );
    const dbsBefore = durableFiles(STORE_DIR);
    assert.ok(dbsBefore.length >= 1, 'the awaiting run committed to a durable per-thread store');
    pass(`run awaiting; durable store on disk: ${dbsBefore.map((f) => f.replace(`${STORE_DIR}/`, '')).join(', ')}`);

    // ---- kill A, start a fresh server B over the SAME storage directory ----
    await stopServer(activeServer);
    activeServer = null;
    const b = spawnServer('real', PORT, { SESSION_DEPLOYMENT_STORAGE_DIR: STORE_DIR, ...realServerEnv('probe', upstream) });
    activeServer = b.server;
    await waitForPort(PORT);
    // The old keep-alive socket died with server A; connect a fresh client to B.
    client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: `http://127.0.0.1:${PORT}` });

    // The durable truth survived the process death (this is the guarantee the
    // store layer provides — ADR-0039 D4 / ADR-0006).
    const dbsAfter = durableFiles(STORE_DIR);
    assert.deepEqual(dbsAfter, dbsBefore, 'the committed durable store survived the restart');
    pass('committed truth persisted on disk across a real process restart');

    // Continuation recovery cause/effect graph: C0 the Session explicitly makes
    // write always_ask while the official Agent-tool default remains allow; C1
    // committed awaiting Run and Session survive; C2 process incarnation
    // changes while the logical owner is stable; C3 confirmation is the first
    // post-restart driving event. Effects: E1 canonical admission advances the
    // realization lease before the billable activity opens; E2 the host recovers
    // the SQLite Run and resumes once; E3 the pre-restart workspace remains
    // visible.
    //
    // | Rule | explicit ask | awaiting truth | new incarnation | first event  | effects    |
    // | R1   | yes          | yes            | yes             | confirmation | E1+E2+E3   |
    const confirmationReceipt = await client.beta.sessions.events.send(session.id, {
      events: [{ type: 'user.tool_confirmation', tool_use_id: toolUse.id, result: 'allow' }],
      betas: BETAS,
    });
    const confirmationReceiptId = confirmationReceipt.data[0]?.id;
    assert.equal(typeof confirmationReceiptId, 'string', 'R1 exact post-restart confirmation receipt');
    const { events: recoveredEvents } = await waitForSessionEventReceipt(
      client,
      session.id,
      confirmationReceiptId,
      BETAS,
      ({ delta }) => delta.some((event) => event.type === 'agent.tool_result')
        && [...delta].reverse().find((event) => event.type === 'session.status_idle')?.stop_reason?.type === 'end_turn',
      'R1 recovered Run to commit after its exact confirmation',
      { timeoutMs: 30_000 },
    );
    const lastIdle = [...recoveredEvents].reverse().find((e) => e.type === 'session.status_idle');
    assert.equal(lastIdle.stop_reason.type, 'end_turn', 'awaiting run resumed and completed after restart');
    const results = recoveredEvents.filter((e) => e.type === 'agent.tool_result');
    assert.ok(
      JSON.stringify(results.at(-1)?.content ?? '').includes('SURVIVE-RESTART'),
      'read-back reflects the pre-restart write — durable state resumed on a fresh process',
    );
    pass('awaiting run resumed from durable truth on a fresh process and completed');
    console.log('E2E PASS: durable cross-restart recovery via TS SDK.');
  } finally {
    if (activeServer) await stopServer(activeServer);
    upstream.close();
    fs.rmSync(STORE_DIR, { recursive: true, force: true });
  }
}

main().catch((err) => {
  console.error('E2E FAIL:', err);
  process.exitCode = 1;
});
