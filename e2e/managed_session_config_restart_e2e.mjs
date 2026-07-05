// Session-config cross-restart e2e (managed session persistence): a session's
// wire config — its agent, title, and metadata — is durable across a real process
// restart, not just its transcript. Before this, a rehydrated session reported a
// placeholder (agent "assistant", no title/metadata); the durable
// ManagedSessionRepository (sessions.db under AWAKEN_MGMT_DIR) now restores the
// real values.
//
// Flow: management mode with BOTH AWAKEN_MGMT_DIR (session config) and
// AWAKEN_STORAGE_DIR (transcript, the rehydration precondition). Create a session
// with a title + metadata, commit a turn, KILL the process, respawn over the same
// dirs, drive one event to trigger lazy rehydration, then retrieve the session and
// assert its config came back — not the placeholder.
//
// Run: (from e2e/)  node managed_session_config_restart_e2e.mjs

import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import Anthropic from '@anthropic-ai/sdk';
import { spawnServer, stopServer, waitForPort, pass } from './harness.mjs';

const BETAS = ['managed-agents-2026-04-01'];
const PORT = Number(process.env.E2E_PORT ?? 38215);
const SEAL_KEY = '00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff';

const client = (base) => new Anthropic({ apiKey: 'e2e-dummy', baseURL: base });

async function listEvents(c, id) {
  const events = [];
  for await (const ev of c.beta.sessions.events.list(id, { betas: BETAS })) events.push(ev);
  return events;
}

async function main() {
  const mgmtDir = fs.mkdtempSync(path.join(os.tmpdir(), 'awaken-sess-mgmt-'));
  const storeDir = fs.mkdtempSync(path.join(os.tmpdir(), 'awaken-sess-store-'));
  const env = {
    AWAKEN_MGMT_DIR: mgmtDir,
    AWAKEN_MGMT_SEAL_KEY: SEAL_KEY,
    AWAKEN_STORAGE_DIR: storeDir,
  };
  let server = null;
  try {
    // ---- lifetime A: create a session with config, commit a turn ----
    const a = spawnServer('management', PORT, env);
    server = a.server;
    await waitForPort(PORT);
    let c = client(a.baseUrl);

    const created = await c.beta.sessions.create({
      agent: 'coder',
      title: 'Durable session',
      metadata: { team: 'research' },
      environment_id: 'env_local',
      betas: BETAS,
    });
    assert.equal(created.title, 'Durable session', 'title accepted at create');
    assert.equal(created.agent.id, 'coder', 'agent id accepted at create');

    // Commit a turn so a durable transcript exists (the rehydration precondition).
    await c.beta.sessions.events.send(created.id, {
      events: [{ type: 'user.message', content: [{ type: 'text', text: 'hello' }] }],
      betas: BETAS,
    });
    const idle = (await listEvents(c, created.id)).find((e) => e.type === 'session.status_idle');
    assert.ok(idle, 'the turn committed and the session went idle');
    pass('session created with title/metadata and a turn committed');

    // ---- kill A, respawn B over the SAME dirs (fresh in-memory cache) ----
    await stopServer(a.server);
    const b = spawnServer('management', PORT, env);
    server = b.server;
    await waitForPort(PORT);
    c = client(b.baseUrl);

    // Drive one event to trigger lazy rehydration on the fresh process: the
    // adapter rebuilds the session from committed truth + the durable session repo.
    await c.beta.sessions.events.send(created.id, {
      events: [{ type: 'user.message', content: [{ type: 'text', text: 'again' }] }],
      betas: BETAS,
    });

    const restored = await c.beta.sessions.retrieve(created.id, { betas: BETAS });
    assert.equal(
      restored.agent.id,
      'coder',
      'agent id restored from the durable session repo (not the "assistant" placeholder)',
    );
    assert.equal(restored.title, 'Durable session', 'title restored across the restart');
    assert.equal(
      restored.metadata?.team,
      'research',
      'metadata restored across the restart',
    );
    pass('session agent/title/metadata restored on a fresh process after restart');
    console.log('E2E PASS: managed session config survives a real restart.');
  } finally {
    if (server) await stopServer(server);
    fs.rmSync(mgmtDir, { recursive: true, force: true });
    fs.rmSync(storeDir, { recursive: true, force: true });
  }
}

main().catch((err) => {
  console.error('E2E FAIL:', err);
  process.exitCode = 1;
});
