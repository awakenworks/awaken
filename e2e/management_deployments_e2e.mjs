// The deployments + deployment-runs families, driven by the official Anthropic
// TypeScript SDK (`client.beta.deployments.*`, `client.beta.deploymentRuns.*`):
// create / retrieve / update / list / archive / pause / unpause / run, and run
// retrieve / list. Any wire-shape drift from the official
// `BetaManagedAgentsDeployment` / `BetaManagedAgentsDeploymentRun` types surfaces
// as an SDK decode error.
//
// Run: (from e2e/)  node management_deployments_e2e.mjs

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
    await withServer('management', 38140, async (baseUrl) => {
      const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: baseUrl });

      const dep = await client.beta.deployments.create({
        agent: 'agent_x',
        environment_id: 'env_1',
        name: 'nightly',
        initial_events: [{ type: 'user.message', content: [{ type: 'text', text: 'go' }] }],
        vault_ids: ['vlt_1'],
        betas: BETAS,
      });
      assert.equal(dep.type, 'deployment');
      assert.ok(dep.id.startsWith('deploy_'), `id: ${dep.id}`);
      assert.equal(dep.status, 'active');
      assert.equal(dep.agent.type, 'agent');
      assert.equal(dep.agent.id, 'agent_x');
      pass('beta.deployments.create -> BetaManagedAgentsDeployment (agent normalized)');

      const got = await client.beta.deployments.retrieve(dep.id, { betas: BETAS });
      assert.equal(got.id, dep.id);
      pass('beta.deployments.retrieve');

      const up = await client.beta.deployments.update(dep.id, { name: 'hourly', betas: BETAS });
      assert.equal(up.name, 'hourly');
      pass('beta.deployments.update');

      const paused = await client.beta.deployments.pause(dep.id, { betas: BETAS });
      assert.equal(paused.status, 'paused');
      assert.equal(paused.paused_reason.type, 'manual');
      const active = await client.beta.deployments.unpause(dep.id, { betas: BETAS });
      assert.equal(active.status, 'active');
      assert.equal(active.paused_reason, null);
      pass('beta.deployments.pause / unpause');

      const run = await client.beta.deployments.run(dep.id, { betas: BETAS });
      assert.equal(run.type, 'deployment_run');
      assert.equal(run.deployment_id, dep.id);
      assert.equal(run.trigger_context.type, 'manual');
      pass('beta.deployments.run -> BetaManagedAgentsDeploymentRun');

      const gotRun = await client.beta.deploymentRuns.retrieve(run.id, { betas: BETAS });
      assert.equal(gotRun.id, run.id);
      const runs = await drain(client.beta.deploymentRuns.list({ deployment_id: dep.id, betas: BETAS }));
      assert.ok(runs.some((r) => r.id === run.id), 'the run is listed');
      pass(`beta.deploymentRuns.retrieve / list (${runs.length})`);

      const archived = await client.beta.deployments.archive(dep.id, { betas: BETAS });
      assert.ok(archived.archived_at, 'archived deployment carries archived_at');
      const ids = (await drain(client.beta.deployments.list({ betas: BETAS }))).map((d) => d.id);
      assert.ok(ids.includes(dep.id));
      pass('beta.deployments.archive / list');
    });

    console.log('E2E PASS: the deployments + deployment-runs families round-trip through the official @anthropic-ai/sdk.');
    process.exitCode = 0;
  } catch (err) {
    console.error('E2E FAIL:', err);
    process.exitCode = 1;
  }
}

main();
