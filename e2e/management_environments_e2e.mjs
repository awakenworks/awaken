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
  // Test design (Environment/work matrix). Causes: C1=create/update fields are
  // omitted, valid, null, or invalid; C2=config/policy unions are canonical,
  // internally inconsistent, or private/unknown; C3=Work is queued, leased,
  // reclaimed, stopped; C4=the
  // Environment is archived. Effects: E1=valid CRUD round-trips one DTO;
  // E2=invalid input is 400 with no resource/work side effect; E3=C3 follows one
  // lease lifecycle; E4=C4 makes update/policy/Work/delete terminally unavailable.
  // Constraints/invariant: one Environment aggregate owns config, policy, and
  // Work availability. Descriptions retain the official nullable contract:
  // create omission/null projects null; update omission preserves, update null
  // clears to null, and an explicit empty string remains distinct.
  // Decision rules: E1=C1(valid)+C2(valid); E2=C1/C2(invalid or inconsistent);
  // E3=E1+C3; E4=E1+C4.
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
      assert.equal(scoped.description, null, 'omitted description projects as official null');
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
        ['self-hosted packages', { type: 'self_hosted', packages: { pip: ['httpx'] } }],
        ['self-hosted networking', { type: 'self_hosted', networking: { type: 'unrestricted' } }],
        ['unknown variant', { type: 'custom_cloud' }],
        ['unknown network field', { type: 'cloud', networking: { type: 'limited', proxy: 'x' } }],
        ['unknown package manager', { type: 'cloud', packages: { docker: ['x'] } }],
        ['host with scheme', { type: 'cloud', networking: { type: 'limited', allowed_hosts: ['https://api.test'] } }],
        ['host with port', { type: 'cloud', networking: { type: 'limited', allowed_hosts: ['api.test:443'] } }],
        ['malformed wildcard', { type: 'cloud', networking: { type: 'limited', allowed_hosts: ['*api.test'] } }],
        ['empty package', { type: 'cloud', packages: { pip: [''] } }],
        ['package option injection', { type: 'cloud', packages: { npm: ['--registry'] } }],
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
          packages: { type: 'packages', apt: null, npm: null },
        },
        betas: BETAS,
      });
      assert.deepEqual(nullableCloud.config.networking.allowed_hosts, []);
      assert.equal(nullableCloud.config.networking.allow_mcp_servers, false);
      assert.equal(nullableCloud.config.networking.allow_package_managers, false);
      assert.deepEqual(nullableCloud.config.packages.apt, []);
      assert.deepEqual(nullableCloud.config.packages.npm, []);
      const deniedPackageNetwork = await fetch(`${baseUrl}/v1/environments`, {
        method: 'POST',
        headers: { 'content-type': 'application/json', 'anthropic-beta': BETAS[0] },
        body: JSON.stringify({
          name: 'packages-without-network-authority',
          config: {
            type: 'cloud',
            networking: { type: 'limited', allow_package_managers: false },
            packages: { type: 'packages', npm: ['tsx'] },
          },
        }),
      });
      assert.equal(deniedPackageNetwork.status, 422, 'cross-field package policy fails closed');

      // Environment update cause graph:
      // a present cloud config patches only present nested fields; omitted
      // networking/package fields retain the durable aggregate value, explicit
      // null resets that field to its canonical default, and the next Session
      // compiles from the resulting exact Environment revision.
      //
      // | Rule | Update field | Value | Durable effect |
      // |---|---|---|---|
      // | U1 | networking members | omitted | preserve hosts/package flag |
      // | U2 | allow_mcp_servers | false | replace only that flag |
      // | U3 | packages.npm | null | clear npm; preserve pip |
      // | U4 | networking | null | reset to unrestricted; preserve packages |
      const patchable = await client.beta.environments.create({
        name: 'patchable-cloud',
        config: {
          type: 'cloud',
          networking: {
            type: 'limited',
            allowed_hosts: ['api.example.test'],
            allow_mcp_servers: true,
            allow_package_managers: true,
          },
          packages: { type: 'packages', npm: ['tsx'], pip: ['httpx'] },
        },
        betas: BETAS,
      });
      const patched = await client.beta.environments.update(patchable.id, {
        config: {
          type: 'cloud',
          networking: { type: 'limited', allow_mcp_servers: false },
          packages: { type: 'packages', npm: null },
        },
        betas: BETAS,
      });
      assert.deepEqual(patched.config.networking.allowed_hosts, ['api.example.test'], 'U1');
      assert.equal(patched.config.networking.allow_mcp_servers, false, 'U2');
      assert.equal(patched.config.networking.allow_package_managers, true, 'U1');
      assert.deepEqual(patched.config.packages.npm, [], 'U3');
      assert.deepEqual(patched.config.packages.pip, ['httpx'], 'U3');
      const resetNetwork = await client.beta.environments.update(patchable.id, {
        config: { type: 'cloud', networking: null },
        betas: BETAS,
      });
      assert.equal(resetNetwork.config.networking.type, 'unrestricted', 'U4');
      assert.deepEqual(resetNetwork.config.packages.pip, ['httpx'], 'U4');
      pass('Environment update preserves omitted fields and resets explicit null fields');
      const namesAfterRejectedCreate = (await drain(
        client.beta.environments.list({ betas: BETAS }),
      )).map((item) => item.name);
      for (const rejectedName of [
        ...rejectedConfigs.map(([name]) => name),
        'must-not-exist',
        'packages-without-network-authority',
      ]) {
        assert.ok(
          !namesAfterRejectedCreate.includes(rejectedName),
          `${rejectedName} must have no durable Environment/work side effect`,
        );
      }
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
      // Exact-policy binding cause/effect rules: P1 missing policy -> 404 with
      // no Environment mutation; P2 active exact version -> bind 200; P3 later
      // publication -> the prior binding remains frozen at v1. This assertion
      // owns P1; the bind/project assertions below own P2/P3.
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
        environment_id: env.id, policy_id: policyId, provisioning: 'eager', version: 1,
      });
      pass('SandboxExecutionPolicy exact-version binding and ownership decision table');

      const gotEnv = await client.beta.environments.retrieve(env.id, { betas: BETAS });
      assert.equal(gotEnv.id, env.id);
      // Description cause/effect graph: C1=create omits or supplies null;
      // C2=update omits description; C3=update supplies a string or null.
      // E1=C1 authors null; E2=C2 preserves the current value; E3=C3 replaces
      // with the exact string or clears to null. K: the Environment aggregate,
      // store, history, and official BetaEnvironment output share one nullable
      // description authority; empty string and null remain distinct facts.
      //
      // Decision table:
      // | Rule | operation | description | durable/projected effect |
      // | D0 | create | omitted/null | null |
      // | D1 | update | omitted | preserve prior value |
      // | D2 | update | string | replace with exact string |
      // | D3 | update | null | clear to null |
      // | D4 | update | empty string | retain exact empty string |
      const upEnv = await client.beta.environments.update(env.id, {
        description: 'updated',
        betas: BETAS,
      });
      assert.equal(upEnv.description, 'updated');
      const preservedDescription = await client.beta.environments.update(env.id, {
        name: 'prod-renamed', betas: BETAS,
      });
      assert.equal(preservedDescription.description, 'updated', 'D1');
      const clearedDescription = await client.beta.environments.update(env.id, {
        description: null, betas: BETAS,
      });
      assert.equal(clearedDescription.description, null, 'D3');
      const emptyDescription = await client.beta.environments.update(env.id, {
        description: '', betas: BETAS,
      });
      assert.equal(emptyDescription.description, '', 'D4');
      assert.equal(
        (await client.beta.environments.retrieve(env.id, { betas: BETAS })).description,
        '',
        'D2-D4 retrieve uses the persisted nullable value',
      );
      const envIds = (await drain(client.beta.environments.list({ betas: BETAS }))).map((e) => e.id);
      assert.ok(envIds.includes(env.id));
      pass('beta.environments.retrieve / update / list + canonical description decision table');

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
      // Terminal lifecycle cause/effect graph: C1 first archive withdraws the
      // current execution projection and purges Work; C2 definition update after
      // archive; C3 exact-policy bind after archive; C4 delete replays archive.
      // Effects: E1 C2/C3 are 409 with no revival, E2 Work stays unavailable,
      // E3 C4 is idempotent and returns the official deleted projection.
      //
      // | Rule | archived | command | status | execution/work effect |
      // | T1 | false | archive | 200 | withdrawn and purged |
      // | T2 | true | update | 409 | none |
      // | T3 | true | bind policy | 409 | none |
      // | T4 | true | delete | 200 | remains withdrawn |
      const archived = await client.beta.environments.archive(env.id, { betas: BETAS });
      assert.ok(archived.archived_at);
      await assert.rejects(
        client.beta.environments.update(env.id, { name: 'must-not-revive', betas: BETAS }),
        (error) => error?.status === 409,
        'T2 archived Environment update is a lifecycle conflict',
      );
      const archivedBind = await fetch(
        `${baseUrl}/v1/awaken/environments/${env.id}/sandbox-execution-policy`,
        {
          method: 'POST',
          headers: { 'content-type': 'application/json' },
          body: JSON.stringify({ policy_id: policyId, version: 1 }),
        },
      );
      assert.equal(archivedBind.status, 409, 'T3 archived policy bind cannot revive execution');
      await assert.rejects(
        client.beta.environments.work.list(env.id, { betas: BETAS }),
        (error) => error?.status === 404,
        'T1/T3 archived Environment Work remains unavailable',
      );
      const del = await client.beta.environments.delete(env.id, { betas: BETAS });
      assert.equal(del.type, 'environment_deleted');
      pass('archive is terminal across update, policy binding, Work, and delete replay');
    });

    console.log('E2E PASS: the environments + work family round-trips through the official @anthropic-ai/sdk.');
    process.exitCode = 0;
  } catch (err) {
    console.error('E2E FAIL:', err);
    process.exitCode = 1;
  }
}

main();
