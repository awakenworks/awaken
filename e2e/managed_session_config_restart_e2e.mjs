// Session-config cross-restart e2e (managed session persistence): a session's
// wire config — its agent, title, and metadata — is durable across a real process
// restart, not just its transcript. Before this, a rehydrated session reported a
// placeholder (agent "assistant", no title/metadata); the durable
// ManagedSessionRepository (sessions.db under typed data_dir) now restores the
// real values.
//
// Cause graph / decision table:
//   C1 config row and transcript committed -> E1 restart can rehydrate
//   C2 quiescent roots copied and restored -> E2 agent/title/metadata preserved
//   C3 explicit no-login fixture identity -> E3 unrelated IAM cannot mask persistence
//
//   Rule  C1  C2  C3  Expected
//   R1    Y   Y   Y   E1 + E2 + E3
//   R2    N   -   Y   no rehydration claim (covered by missing-session tests)
//   R3    Y   N   Y   no restore claim (covered by repository isolation)
//
// Flow: management mode with BOTH typed data_dir (session config) and
// SESSION_DEPLOYMENT_STORAGE_DIR (transcript, the rehydration precondition). Create a session
// with a title + metadata, commit a turn, KILL the process, respawn over the same
// copied roots, first shadow-read the candidate, drive one event to prove
// continuation, then assert its config came back — not the placeholder.
//
// Run: (from e2e/)  node managed_session_config_restart_e2e.mjs

import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import Anthropic from '@anthropic-ai/sdk';
import {
  deploymentEnv,
  spawnServer,
  stopServer,
  waitForPort,
  pass,
  startUpstream,
  realServerEnv,
  waitForSessionEventReceipt,
  copyQuiescentTree,
  publishManagementAgent,
  bindSandboxExecutionPolicy,
} from './harness.mjs';

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
  const restoredMgmtDir = `${mgmtDir}-restored`;
  const restoredStoreDir = `${storeDir}-restored`;
  const originalEnv = {
    ...deploymentEnv(mgmtDir, { identityMode: 'no-login', controlSealKey: SEAL_KEY }),
    SESSION_DEPLOYMENT_STORAGE_DIR: storeDir,
  };
  const upstream = await startUpstream('mcp');
  let server = null;
  try {
    // ---- lifetime A: create a session with config, commit a turn ----
    const a = spawnServer('management', PORT, { ...originalEnv, ...realServerEnv('mcp', upstream, { mode: 'management' }) });
    server = a.server;
    await waitForPort(PORT);
    let c = client(a.baseUrl);

    // The Config publication is the execution authority for the non-default
    // Agent. Merely accepting `agent: coder` in the Session DTO must not invent
    // an executable publication or fall back to `assistant`.
    await publishManagementAgent(a.baseUrl, 'coder', {
      name: 'Durable coder fixture',
      system: 'Reply normally.',
      max_steps: 4,
      model: {
        mode: 'pinned',
        provider_identity_ref: 'default',
        model_ref: 'fake-haiku',
        backend_ref: 'default',
      },
      tools: [],
    });
    const environment = await c.beta.environments.create({
      name: 'Portable lazy environment',
      config: { type: 'self_hosted' },
      betas: BETAS,
    });
    await bindSandboxExecutionPolicy(a.baseUrl, environment.id, {
      id: `portable-lazy-${process.pid}`,
      provisioning: 'on_tool_use',
    });

    const created = await c.beta.sessions.create({
      agent: 'coder',
      title: 'Durable session',
      metadata: { team: 'research' },
      environment_id: environment.id,
      betas: BETAS,
    });
    assert.equal(created.title, 'Durable session', 'title accepted at create');
    assert.equal(created.agent.id, 'coder', 'agent id accepted at create');

    // Commit a turn so a durable transcript exists (the rehydration precondition).
    const firstReceipt = await c.beta.sessions.events.send(created.id, {
      events: [{ type: 'user.message', content: [{ type: 'text', text: 'hello' }] }],
      betas: BETAS,
    });
    const firstReceiptId = firstReceipt.data[0]?.id;
    assert.equal(typeof firstReceiptId, 'string', 'R1 exact pre-restart User Event receipt');
    const firstRun = await waitForSessionEventReceipt(
      c,
      created.id,
      firstReceiptId,
      BETAS,
      ({ delta }) => delta.some((event) => event.type === 'session.status_idle'),
      'R1 pre-restart Run to commit',
    );
    const idle = firstRun.delta.find((e) => e.type === 'session.status_idle');
    assert.ok(idle, 'the turn committed and the session went idle');
    pass('session created with title/metadata and a turn committed');

    // ---- quiesce A, copy backups to NEW roots, respawn B from them ----
    await stopServer(a.server);
    const portableBackup = {
      // Filesystem identities deliberately do not survive copies. Excluding
      // physical realizations makes the existing typed-unavailability path
      // rebuild a fresh incarnation instead of weakening substitution checks.
      excludeTopLevel: ['sandboxes', 'trusted-local-sandboxes'],
    };
    copyQuiescentTree(mgmtDir, restoredMgmtDir, portableBackup);
    copyQuiescentTree(storeDir, restoredStoreDir, portableBackup);
    const restoredEnv = {
      ...deploymentEnv(restoredMgmtDir, {
        identityMode: 'no-login',
        controlSealKey: SEAL_KEY,
      }),
      SESSION_DEPLOYMENT_STORAGE_DIR: restoredStoreDir,
    };
    const b = spawnServer('management', PORT, { ...restoredEnv, ...realServerEnv('mcp', upstream, { mode: 'management' }) });
    server = b.server;
    await waitForPort(PORT);
    c = client(b.baseUrl);

    // Backup/shadow cause-effect graph: C1 A is quiesced after config and
    // transcript commit; C2 both roots are copied to fresh destinations; C3 B
    // begins with an empty memory cache. Effects: E1 a read-only candidate
    // projection reconstructs exact durable config without touching the source;
    // E2 a later Event continues from the restored committed prefix. Decision
    // B1=C1+C2+C3->E1; B2=B1+accepted Event->E2.
    const shadow = await c.beta.sessions.retrieve(created.id, { betas: BETAS });
    assert.equal(shadow.agent.id, 'coder', 'B1 shadow replay agent');
    assert.equal(shadow.title, 'Durable session', 'B1 shadow replay title');
    assert.equal(shadow.metadata?.team, 'research', 'B1 shadow replay metadata');
    pass('quiescent backup restores an exact read-only candidate projection');

    // Drive one event to trigger lazy rehydration on the fresh process: the
    // adapter rebuilds the session from committed truth + the durable session repo.
    // C1=durable config+transcript; C2=fresh process; C3=exact follow-up receipt;
    // E1=post-C3 terminal proves rehydration before projection assertions.
    // K: retrieve observes the repository and does not trigger lifecycle work.
    // Decision R1 C1+C2+C3&&!E1=>retry; R2 all=>assert durable config.
    const secondReceipt = await c.beta.sessions.events.send(created.id, {
      events: [{ type: 'user.message', content: [{ type: 'text', text: 'again' }] }],
      betas: BETAS,
    });
    const secondReceiptId = secondReceipt.data[0]?.id;
    assert.equal(typeof secondReceiptId, 'string', 'R2 exact post-restart User Event receipt');
    await waitForSessionEventReceipt(
      c,
      created.id,
      secondReceiptId,
      BETAS,
      ({ delta }) => delta.some((event) => event.type === 'session.status_idle'),
      'R2 post-restart Run to commit after rehydration',
    );

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
    upstream.close();
    fs.rmSync(mgmtDir, { recursive: true, force: true });
    fs.rmSync(storeDir, { recursive: true, force: true });
    fs.rmSync(restoredMgmtDir, { recursive: true, force: true });
    fs.rmSync(restoredStoreDir, { recursive: true, force: true });
  }
}

main().catch((err) => {
  console.error('E2E FAIL:', err);
  process.exitCode = 1;
});
