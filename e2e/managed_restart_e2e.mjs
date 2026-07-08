// Durable cross-restart recovery across a real process restart, via the official
// Anthropic TS SDK. A mutating tool parks for approval; its waiting ticket +
// transcript commit to a per-thread durable store under AWAKEN_STORAGE_DIR (SQLite
// by default, the filesystem append-log with AWAKEN_STORE=fs). We KILL the server
// process and start a fresh one over the same storage directory, then approve on
// the SAME session with a freshly connected client. The rebuilt process has no
// in-memory session state, so the managed adapter rehydrates the session from
// committed truth (ADR-0039 lazy session rehydration) and the host recovers the
// parked run from the store — the run resumes and completes end-to-end. This
// exercises the durable commit + hydrate + fact-authority read path through HTTP.
//
// Run: (from e2e/)  node managed_restart_e2e.mjs   (add AWAKEN_STORE=fs for fs)

import assert from 'node:assert/strict';
import fs from 'node:fs';
import Anthropic from '@anthropic-ai/sdk';
import { spawnServer, stopServer, waitForPort, pass, startUpstream, realServerEnv } from './harness.mjs';

const PORT = Number(process.env.E2E_PORT ?? 38130);
const BETAS = ['managed-agents-2026-04-01'];
const STORE_DIR = `/tmp/awaken-restart-e2e-${process.pid}`;

// `let`, not `const`: after the server restart the old keep-alive socket is dead,
// so the post-restart calls use a freshly connected client (see below).
let client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: `http://127.0.0.1:${PORT}` });

const listEvents = async (sessionId) => {
  const events = [];
  for await (const ev of client.beta.sessions.events.list(sessionId, { betas: BETAS })) events.push(ev);
  return events;
};

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

  // ---- server A: start a run that parks on a tool confirmation ----
  const upstream = await startUpstream('probe');
  const a = spawnServer('real', PORT, { AWAKEN_STORAGE_DIR: STORE_DIR, ...realServerEnv('probe', upstream) });
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
  const dbsBefore = durableFiles(STORE_DIR);
  assert.ok(dbsBefore.length >= 1, 'the parked run committed to a durable per-thread store');
  pass(`run parked; durable store on disk: ${dbsBefore.map((f) => f.replace(`${STORE_DIR}/`, '')).join(', ')}`);

  // ---- kill A, start a fresh server B over the SAME storage directory ----
  await stopServer(a.server);
  const b = spawnServer('real', PORT, { AWAKEN_STORAGE_DIR: STORE_DIR, ...realServerEnv('probe', upstream) });
  await waitForPort(PORT);
  // The old keep-alive socket died with server A; connect a fresh client to B.
  client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: `http://127.0.0.1:${PORT}` });

  // The durable truth survived the process death (this is the guarantee the
  // store layer provides — ADR-0039 D4 / ADR-0006).
  const dbsAfter = durableFiles(STORE_DIR);
  assert.deepEqual(dbsAfter, dbsBefore, 'the committed durable store survived the restart');
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
    upstream.close();
    fs.rmSync(STORE_DIR, { recursive: true, force: true });
  }
}

main().catch((err) => {
  console.error('E2E FAIL:', err);
  process.exitCode = 1;
});
