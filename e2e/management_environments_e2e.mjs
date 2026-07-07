// The environments + work-queue family, driven by the official Anthropic
// TypeScript SDK (`client.beta.environments.*`, `.work.*`): env CRUD + archive,
// and the work lifecycle (list / poll / retrieve / update / ack / heartbeat /
// stop / stats). Any wire-shape drift from the official `BetaEnvironment` /
// `BetaSelfHostedWork` / `BetaSelfHostedWorkQueueStats` types surfaces as an SDK
// decode error.
//
// Run: (from e2e/)  node management_environments_e2e.mjs

import assert from 'node:assert/strict';
import Anthropic from '@anthropic-ai/sdk';
import { withServer, pass } from './harness.mjs';

const BETAS = ['managed-agents-2026-04-01'];

async function drain(pagePromise) {
  const items = [];
  for await (const item of pagePromise) items.push(item);
  return items;
}

async function main() {
  try {
    await withServer('management', 38148, async (baseUrl) => {
      const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: baseUrl });

      // -- Environment CRUD --------------------------------------------------
      const env = await client.beta.environments.create({
        name: 'prod',
        config: { type: 'self_hosted' },
        description: 'self-hosted workers',
        betas: BETAS,
      });
      assert.equal(env.type, 'environment');
      assert.equal(env.config.type, 'self_hosted');
      assert.ok(env.id.startsWith('env_'), `id: ${env.id}`);
      pass('beta.environments.create -> BetaEnvironment');

      // Byte-faithful to the official BetaEnvironment: ownership is credential-
      // implicit, so the object carries no `scope`, and a `scope` sent in the body
      // is ignored (non-official field) rather than echoed.
      assert.equal(env.scope, undefined, 'SDK-created environment carries no scope');
      const rawRes = await fetch(`${baseUrl}/v1/environments`, {
        method: 'POST',
        headers: { 'content-type': 'application/json', 'anthropic-beta': BETAS[0] },
        body: JSON.stringify({ name: 'raw-scoped', scope: 'org_acme/ws_eng/proj_x' }),
      });
      assert.equal(rawRes.status, 200);
      const rawEnv = await rawRes.json();
      assert.ok(!('scope' in rawEnv), 'a body scope is ignored, not echoed on the wire');
      pass('environment is byte-faithful: no scope field, body scope ignored');

      const gotEnv = await client.beta.environments.retrieve(env.id, { betas: BETAS });
      assert.equal(gotEnv.id, env.id);
      const upEnv = await client.beta.environments.update(env.id, {
        description: 'updated',
        betas: BETAS,
      });
      assert.equal(upEnv.description, 'updated');
      const envIds = (await drain(client.beta.environments.list({ betas: BETAS }))).map((e) => e.id);
      assert.ok(envIds.includes(env.id));
      pass('beta.environments.retrieve / update / list');

      // -- Work queue --------------------------------------------------------
      const seeded = await drain(client.beta.environments.work.list(env.id, { betas: BETAS }));
      assert.equal(seeded.length, 1, 'a fresh environment is seeded with one work item');
      assert.equal(seeded[0].data.type, 'healthcheck');

      const stats = await client.beta.environments.work.stats(env.id, { betas: BETAS });
      assert.equal(stats.type, 'work_queue_stats');
      assert.equal(stats.depth, 1);
      pass('beta.environments.work.list / stats');

      const leased = await client.beta.environments.work.poll(env.id, { betas: BETAS });
      assert.ok(leased, 'poll leases the queued item');
      assert.equal(leased.state, 'active');
      const wid = leased.id;

      // Second poll -> null (single-worker cap).
      const again = await client.beta.environments.work.poll(env.id, { betas: BETAS });
      assert.equal(again, null, 'only one active lease at a time (single-worker cap)');
      pass('beta.environments.work.poll -> lease, then null (single-worker cap)');

      const gotWork = await client.beta.environments.work.retrieve(wid, {
        environment_id: env.id,
        betas: BETAS,
      });
      assert.equal(gotWork.id, wid);

      const acked = await client.beta.environments.work.ack(wid, {
        environment_id: env.id,
        betas: BETAS,
      });
      assert.ok(acked.acknowledged_at);

      const hb = await client.beta.environments.work.heartbeat(wid, {
        environment_id: env.id,
        betas: BETAS,
      });
      assert.equal(hb.type, 'work_heartbeat');
      assert.equal(hb.lease_extended, true);

      const updWork = await client.beta.environments.work.update(wid, {
        environment_id: env.id,
        metadata: { run: '1' },
        betas: BETAS,
      });
      assert.equal(updWork.metadata.run, '1');

      const stopped = await client.beta.environments.work.stop(wid, {
        environment_id: env.id,
        betas: BETAS,
      });
      assert.equal(stopped.state, 'stopped');
      pass('beta.environments.work.retrieve / ack / heartbeat / update / stop');

      // -- Archive + delete --------------------------------------------------
      const archived = await client.beta.environments.archive(env.id, { betas: BETAS });
      assert.ok(archived.archived_at);
      const del = await client.beta.environments.delete(env.id, { betas: BETAS });
      assert.equal(del.type, 'environment_deleted');
      pass('beta.environments.archive / delete');
    });

    console.log('E2E PASS: the environments + work family round-trips through the official @anthropic-ai/sdk.');
    process.exitCode = 0;
  } catch (err) {
    console.error('E2E FAIL:', err);
    process.exitCode = 1;
  }
}

main();
