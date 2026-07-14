// Durable-deployment persistence + lifecycle for the self-hosted environment
// registry + work queue (the unify-work-lease durable backends). Spawn
// awaken-server in `management` mode over a fixed AWAKEN_MGMT_DIR (SQLite
// backends), drive the official Anthropic SDK, and assert:
//
//   1. Enqueue semantics: creating a self-hosted environment seeds a `healthcheck`
//      work item; creating a session ON that environment enqueues one `session`
//      work item (its data.id is the session id).
//   2. No re-enqueue on subsequent messages: sending more turns to the session does
//      NOT add work items — work is a session-lifecycle signal (create / dormant
//      wake), not a per-message queue (that is dispatch's job).
//   3. Durability: kill the process, respawn over the same dir — the environment
//      (with its metadata mutation) AND both work items survive and are claimable.
//
// This is the durable path the in-memory default never exercised.
//
// Run: (from e2e/)  node management_env_work_persistence_e2e.mjs

import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import Anthropic from '@anthropic-ai/sdk';
import { spawnServer, stopServer, waitForPort, pass, startUpstream, realServerEnv } from './harness.mjs';

const BETAS = ['managed-agents-2026-04-01'];
const PORT = Number(process.env.E2E_PORT ?? 38294);
const SEAL_KEY = '00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff';

async function drain(pagePromise) {
  const items = [];
  for await (const item of pagePromise) items.push(item);
  return items;
}

function boot(env, upstream) {
  const { server, baseUrl } = spawnServer('management', PORT, {
    ...env,
    ...realServerEnv('echo', upstream, { mode: 'management' }),
  });
  return { server, baseUrl };
}

const kinds = (work) => work.map((w) => w.data.type).sort();

async function main() {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'awaken-envwork-e2e-'));
  const env = { AWAKEN_MGMT_DIR: dir, AWAKEN_MGMT_SEAL_KEY: SEAL_KEY };
  const upstream = await startUpstream('echo');
  let server = null;
  try {
    // ---- lifetime A: create env + session, prove enqueue semantics ---------
    let boot_a = boot(env, upstream);
    server = boot_a.server;
    await waitForPort(PORT);
    let client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: boot_a.baseUrl });

    const created = await client.beta.environments.create({
      name: 'prod',
      config: { type: 'self_hosted' },
      metadata: { team: 'research' },
      betas: BETAS,
    });
    // Creating the environment seeds one `healthcheck` work item.
    let work = await drain(client.beta.environments.work.list(created.id, { betas: BETAS }));
    assert.deepEqual(kinds(work), ['healthcheck'], 'env create seeds one healthcheck');

    // Creating a session ON the self-hosted environment enqueues `session` work.
    const session = await client.beta.sessions.create({
      agent: 'assistant',
      environment_id: created.id,
      betas: BETAS,
    });
    work = await drain(client.beta.environments.work.list(created.id, { betas: BETAS }));
    assert.deepEqual(kinds(work), ['healthcheck', 'session'], 'session create enqueues session work');
    const sessionWork = work.find((w) => w.data.type === 'session');
    assert.equal(sessionWork.data.id, session.id, 'the session work references the session id');
    pass('env create seeds healthcheck; session create enqueues one session work item');

    // Work is per session-creation, not per message: a SECOND session on the same
    // environment enqueues a SECOND session work item (one work per session — a
    // self-hosted session is executed by the external worker, so its later turns
    // flow over the session events API, never as new work).
    const session2 = await client.beta.sessions.create({
      agent: 'assistant',
      environment_id: created.id,
      betas: BETAS,
    });
    work = await drain(client.beta.environments.work.list(created.id, { betas: BETAS }));
    assert.deepEqual(
      kinds(work),
      ['healthcheck', 'session', 'session'],
      'each session-create enqueues exactly one session work',
    );
    assert.ok(
      work.some((w) => w.data.type === 'session' && w.data.id === session2.id),
      'the second session has its own work item',
    );
    pass('work is a per-session-creation signal (two sessions → two session work items)');

    // A metadata mutation to prove the registry update persists too.
    await client.beta.environments.update(created.id, {
      metadata: { team: 'platform', tier: 'prod' },
      betas: BETAS,
    });

    // ---- restart: kill the process, respawn over the same dir -------------
    await stopServer(server);
    const boot_b = boot(env, upstream);
    server = boot_b.server;
    await waitForPort(PORT);
    client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: boot_b.baseUrl });
    pass('server killed and respawned on the same AWAKEN_MGMT_DIR (SQLite backends)');

    // ---- the environment registry survived, with the mutation -------------
    const after = await client.beta.environments.retrieve(created.id, { betas: BETAS });
    assert.equal(after.id, created.id, 'environment id survives the restart');
    assert.equal(after.metadata.team, 'platform', 'the metadata mutation survives');
    assert.equal(after.metadata.tier, 'prod', 'the added metadata key survives');
    const listed = await drain(client.beta.environments.list({ betas: BETAS }));
    assert.ok(listed.some((e) => e.id === created.id), 'environment still listed after restart');
    pass('environment registry row (with its update) survived the restart');

    // ---- all work items survived, still queued and claimable ---------------
    work = await drain(client.beta.environments.work.list(created.id, { betas: BETAS }));
    assert.deepEqual(
      kinds(work),
      ['healthcheck', 'session', 'session'],
      'all work items survive the restart',
    );
    assert.ok(work.every((w) => w.state === 'queued'), 'surviving work is still queued');
    assert.ok(
      work.some((w) => w.data.type === 'session' && w.data.id === session.id),
      'session work still points at its session',
    );
    const claimed = await client.beta.environments.work.poll(created.id, { betas: BETAS });
    assert.ok(claimed, 'a surviving work item is claimable after restart');
    assert.equal(claimed.state, 'active', 'poll leases the surviving work');
    pass('healthcheck + session work survived the restart and are claimable (SQLite lease)');

    console.log('E2E PASS: env registry + work queue (enqueue/no-re-enqueue/durability) over SQLite.');
  } finally {
    if (server) await stopServer(server);
    upstream.close();
    fs.rmSync(dir, { recursive: true, force: true });
  }
}

main().catch((err) => {
  console.error('E2E FAIL:', err);
  process.exitCode = 1;
});
