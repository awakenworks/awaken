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
import { withScenarioServer, pass } from './harness.mjs';

const BETAS = ['managed-agents-2026-04-01'];

async function drain(pagePromise) {
  const items = [];
  for await (const item of pagePromise) items.push(item);
  return items;
}

async function main() {
  try {
    await withScenarioServer('management', 'mcp', 38148, async (baseUrl) => {
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

      // Scope decision table: omitted is absent; organization/account round-trip;
      // an unknown scope fails before Environment creation.
      assert.equal(env.scope, undefined, 'SDK-created environment carries no scope');
      const scoped = await client.beta.environments.create({
        name: 'scoped', scope: 'organization', betas: BETAS,
      });
      assert.equal(scoped.scope, 'organization');
      const scopedUpdate = await client.beta.environments.update(scoped.id, {
        scope: 'account', betas: BETAS,
      });
      assert.equal(scopedUpdate.scope, 'account');
      const invalidScope = await fetch(`${baseUrl}/v1/environments`, {
        method: 'POST',
        headers: { 'content-type': 'application/json', 'anthropic-beta': BETAS[0] },
        body: JSON.stringify({ name: 'bad-scope', scope: 'workspace' }),
      });
      assert.equal(invalidScope.status, 400);
      pass('official Environment scope omission/create/update/rejection decision table');

      // Environment-config admission cause graph:
      // official tagged union + official nested fields -> canonical resource;
      // any private sandbox/unknown variant/unknown nested field -> 400 before
      // an Environment or healthcheck work item can be created.
      // Decision table:
      // | tagged config | fields                 | admission | durable effect       |
      // | self_hosted   | official only          | accept    | env + healthcheck     |
      // | cloud         | omitted/null optionals | accept    | canonical defaults    |
      // | either        | unknown/private field  | reject    | no env and no work     |
      const rejectedConfigs = [
        ['private sandbox', { type: 'self_hosted', sandbox: { isolation: 'container' } }],
        ['unknown variant', { type: 'custom_cloud' }],
        ['unknown network field', { type: 'cloud', networking: { type: 'limited', proxy: 'x' } }],
        ['unknown package manager', { type: 'cloud', packages: { docker: ['x'] } }],
      ];
      for (const [rule, config] of rejectedConfigs) {
        const response = await fetch(`${baseUrl}/v1/environments`, {
          method: 'POST',
          headers: { 'content-type': 'application/json', 'anthropic-beta': BETAS[0] },
          body: JSON.stringify({ name: rule, config }),
        });
        assert.equal(response.status, 400, rule);
      }
      const unknownCreateField = await fetch(`${baseUrl}/v1/environments`, {
        method: 'POST',
        headers: { 'content-type': 'application/json', 'anthropic-beta': BETAS[0] },
        body: JSON.stringify({ name: 'must-not-exist', execution_policy: 'parallel-owner' }),
      });
      assert.equal(unknownCreateField.status, 400);
      const cloud = await client.beta.environments.create({
        name: 'official-cloud-defaults',
        config: { type: 'cloud' },
        betas: BETAS,
      });
      assert.equal(cloud.config.networking.type, 'unrestricted');
      for (const manager of ['apt', 'cargo', 'gem', 'go', 'npm', 'pip']) {
        assert.deepEqual(cloud.config.packages[manager], [], manager);
      }
      const nullableCloud = await client.beta.environments.create({
        name: 'official-cloud-null-defaults',
        config: {
          type: 'cloud',
          networking: {
            type: 'limited', allowed_hosts: null,
            allow_mcp_servers: null, allow_package_managers: null,
          },
          packages: { type: 'packages', apt: null, npm: ['tsx'] },
        },
        betas: BETAS,
      });
      assert.deepEqual(nullableCloud.config.networking.allowed_hosts, []);
      assert.equal(nullableCloud.config.networking.allow_mcp_servers, false);
      assert.equal(nullableCloud.config.networking.allow_package_managers, false);
      assert.deepEqual(nullableCloud.config.packages.apt, []);
      assert.deepEqual(nullableCloud.config.packages.npm, ['tsx']);
      const namesAfterRejectedCreate = (await drain(
        client.beta.environments.list({ betas: BETAS }),
      )).map((item) => item.name);
      assert.ok(!namesAfterRejectedCreate.includes('must-not-exist'));
      pass('official Environment config union accepts canonical cases and rejects extensions');

      // Awaken sandbox policy is a separate, versioned aggregate. The Environment
      // carries an exact reference, so publishing v2 cannot silently move a v1
      // binding. Networking is rejected here because the official Environment
      // contract remains its sole owner.
      const policyHeaders = { 'content-type': 'application/json' };
      const policyId = `strict-${process.pid}-${Date.now()}`;
      const createdPolicy = await fetch(`${baseUrl}/v1/awaken/sandbox-execution-policies`, {
        method: 'POST', headers: policyHeaders,
        body: JSON.stringify({ id: policyId, config: { isolation: 'namespace', limits: { cpu_millis: 500 } } }),
      });
      assert.equal(createdPolicy.status, 201);
      const overlappingNetwork = await fetch(`${baseUrl}/v1/awaken/sandbox-execution-policies`, {
        method: 'POST', headers: policyHeaders,
        body: JSON.stringify({ id: `${policyId}-overlap`, config: { network: { mode: 'none' } } }),
      });
      assert.equal(overlappingNetwork.status, 422);
      const unknownPolicyField = await fetch(`${baseUrl}/v1/awaken/sandbox-execution-policies`, {
        method: 'POST', headers: policyHeaders,
        body: JSON.stringify({ id: `${policyId}-unknown`, config: { image: 'implicit:latest' } }),
      });
      assert.ok([400, 422].includes(unknownPolicyField.status));
      const missingBinding = await fetch(`${baseUrl}/v1/awaken/environments/${env.id}/sandbox-execution-policy`, {
        method: 'POST', headers: policyHeaders,
        body: JSON.stringify({ policy_id: 'missing', version: 1 }),
      });
      assert.equal(missingBinding.status, 404);
      const bound = await fetch(`${baseUrl}/v1/awaken/environments/${env.id}/sandbox-execution-policy`, {
        method: 'POST', headers: policyHeaders,
        body: JSON.stringify({ policy_id: policyId, version: 1 }),
      });
      assert.equal(bound.status, 200);
      const published = await fetch(`${baseUrl}/v1/awaken/sandbox-execution-policies/${policyId}/versions`, {
        method: 'POST', headers: policyHeaders,
        body: JSON.stringify({ expected_current: 1, config: { isolation: 'container' } }),
      });
      assert.equal(published.status, 200);
      const exactBinding = await fetch(`${baseUrl}/v1/awaken/environments/${env.id}/sandbox-execution-policy`);
      assert.deepEqual(await exactBinding.json(), {
        environment_id: env.id, policy_id: policyId, version: 1,
      });
      pass('SandboxExecutionPolicy exact-version binding and ownership decision table');

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

      const reclaimEnv = await client.beta.environments.create({
        name: 'reclaim-env', config: { type: 'self_hosted' }, betas: BETAS,
      });
      const abandoned = await client.beta.environments.work.poll(reclaimEnv.id, { betas: BETAS });
      const reclaimedResponse = await fetch(
        `${baseUrl}/v1/environments/${reclaimEnv.id}/work/poll?reclaim_older_than_ms=0`,
        { headers: { 'anthropic-worker-id': 'replacement-worker', 'anthropic-beta': BETAS[0] } },
      );
      assert.equal(reclaimedResponse.status, 200);
      const reclaimed = await reclaimedResponse.json();
      assert.equal(reclaimed?.id, abandoned.id, 'an expired lease is re-claimable by another worker');
      assert.equal(reclaimed?.state, 'active');
      pass('reclaim_older_than_ms=0 reclaims an abandoned lease');

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
