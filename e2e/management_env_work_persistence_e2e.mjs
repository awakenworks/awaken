// Durable-deployment persistence for the self-hosted environment registry + work
// queue (the unify-work-lease durable backends). Spawn awaken-server in
// `management` mode over a fixed AWAKEN_MGMT_DIR (SQLite backends), create a
// self-hosted environment through the official Anthropic SDK — which seeds a
// `healthcheck` work item and (with a session) `session` work — mutate it, kill the
// process, respawn it over the same dir, and assert the environment AND its work
// queue survive: the registry row is still there (with the mutation), the work is
// still queued and claimable. This is the durable path the in-memory default never
// exercised.
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

async function main() {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'awaken-envwork-e2e-'));
  const env = { AWAKEN_MGMT_DIR: dir, AWAKEN_MGMT_SEAL_KEY: SEAL_KEY };
  const upstream = await startUpstream('echo');
  let server = null;
  try {
    // ---- lifetime A: create + mutate a self-hosted environment ------------
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
    assert.equal(created.config.type, 'self_hosted');
    // Creating the environment seeds one `healthcheck` work item.
    const seeded = await drain(client.beta.environments.work.list(created.id, { betas: BETAS }));
    assert.equal(seeded.length, 1, `one seeded work item, got ${seeded.length}`);
    assert.equal(seeded[0].data.type, 'healthcheck');
    const workId = seeded[0].id;
    // Mutate the environment's metadata (exercises the registry's durable update).
    await client.beta.environments.update(created.id, {
      metadata: { team: 'platform', tier: 'prod' },
      betas: BETAS,
    });
    pass('created a self-hosted environment + seeded work, then mutated its metadata');

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
    assert.equal(after.name, 'prod', 'environment name survives');
    assert.equal(after.metadata.team, 'platform', 'the metadata mutation survives');
    assert.equal(after.metadata.tier, 'prod', 'the added metadata key survives');
    // It is still listed as active.
    const list = await drain(client.beta.environments.list({ betas: BETAS }));
    assert.ok(list.some((e) => e.id === created.id), 'environment is still listed after restart');
    pass('environment registry row (with its update) survived the restart');

    // ---- the work queue survived, still queued and claimable --------------
    const work = await drain(client.beta.environments.work.list(created.id, { betas: BETAS }));
    assert.equal(work.length, 1, `the work item survives, got ${work.length}`);
    assert.equal(work[0].id, workId, 'the same work id survives');
    assert.equal(work[0].state, 'queued', 'the work is still queued after restart');
    const claimed = await client.beta.environments.work.poll(created.id, { betas: BETAS });
    assert.ok(claimed, 'the surviving work is claimable after restart');
    assert.equal(claimed.id, workId);
    assert.equal(claimed.state, 'active', 'poll leases the surviving work');
    pass('work queue item survived the restart and is claimable (SQLite lease)');

    console.log('E2E PASS: env registry + work queue survive a server restart over SQLite.');
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
