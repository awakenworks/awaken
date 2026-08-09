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
import { withScenarioServer, pass } from './harness.mjs';

/**
 * Causal graph
 *
 * typed deployment config -> durable deployment -> manual/scheduled trigger
 *      |                                              |
 *      + metadata patch/null clear                    v
 *      + schedule replace/null clear          create Session -> send initial event
 *      + resource full replacement                    |
 *                                                     v
 *                                            deployment run links Session
 *
 * Decision table
 *
 * | case | initial event | metadata update | schedule update | behavior |
 * |------|---------------|-----------------|-----------------|----------|
 * | D1   | valid user    | absent          | absent          | create active deployment |
 * | D2   | retained      | upsert + delete | absent          | patch keys, preserve others |
 * | D3   | retained      | absent          | null            | clear schedule, remain active |
 * | D4   | valid user    | absent          | absent          | run creates linked Session |
 * | D5   | invalid union | any             | any             | admission rejects; no deployment |
 * | D6   | archived env  | absent          | absent          | failed run only; manual trigger does not pause |
 * | D7   | mixed runs    | absent          | absent          | typed error/trigger/time filters select exact rows |
 * | D8   | malformed query | absent        | absent          | 400; run store remains unchanged |
 * | D9   | boundary violation | any         | any             | 400; deployment aggregate unchanged |
 * | D10  | list filters | absent           | absent          | exact active/paused/archive/agent/time partition |
 *
 * Causes: authoritative Agent/Environment state, create/update inputs,
 * lifecycle operation, trigger result, and list-filter values.
 * Constraints: every accepted Deployment binds one existing active Agent
 * version and one Workspace-owned Environment before mutation.
 * Effects: typed Deployment/Run projections, one ordinary Session on a valid
 * trigger, atomic rejection, terminal archive, and exact list partitions.
 * Decision rules: D1-D10 above cover the public CRUD, manual-trigger, failure,
 * boundary, and filter combinations owned by this official SDK suite.
 *
 * Assertions below target lifecycle and mutation effects through the official SDK,
 * not only response decoding.
 */

const BETAS = ['managed-agents-2026-04-01'];

async function drain(pagePromise) {
  const items = [];
  for await (const item of pagePromise) items.push(item);
  return items;
}

async function main() {
  try {
    await withScenarioServer('management', 'mcp', 38140, async (baseUrl) => {
      const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: baseUrl });
      const environment = await client.beta.environments.create({
        name: 'deployment-e2e',
        config: { type: 'cloud' },
        betas: BETAS,
      });
      const deploymentAgent = await client.beta.agents.create({
        name: 'deployment-e2e-agent',
        model: 'claude-opus-4-8',
        betas: BETAS,
      });

      const dep = await client.beta.deployments.create({
        agent: deploymentAgent.id,
        environment_id: environment.id,
        name: 'nightly',
        description: 'scheduled work',
        metadata: { keep: 'yes', drop: 'old' },
        initial_events: [{ type: 'user.message', content: [{ type: 'text', text: 'go' }] }],
        schedule: { type: 'cron', expression: '0 9 * * 1-5', timezone: 'UTC' },
        vault_ids: ['vlt_1'],
        betas: BETAS,
      });
      assert.equal(dep.type, 'deployment');
      assert.ok(dep.id.startsWith('depl_'), `id: ${dep.id}`);
      assert.equal(dep.status, 'active');
      assert.equal(dep.agent.type, 'agent');
      assert.equal(dep.agent.id, deploymentAgent.id);
      pass('beta.deployments.create -> BetaManagedAgentsDeployment (agent normalized)');

      const got = await client.beta.deployments.retrieve(dep.id, { betas: BETAS });
      assert.equal(got.id, dep.id);
      pass('beta.deployments.retrieve');

      const up = await client.beta.deployments.update(dep.id, {
        name: 'hourly',
        metadata: { drop: null, add: 'new' },
        betas: BETAS,
      });
      assert.equal(up.name, 'hourly');
      assert.deepEqual(up.metadata, { add: 'new', keep: 'yes' });
      assert.equal(up.schedule?.expression, '0 9 * * 1-5');
      pass('beta.deployments.update');

      const cleared = await client.beta.deployments.update(dep.id, {
        description: null,
        schedule: null,
        resources: null,
        vault_ids: null,
        betas: BETAS,
      });
      assert.equal(cleared.description, null);
      assert.equal(cleared.schedule, null);
      assert.deepEqual(cleared.resources, []);
      assert.deepEqual(cleared.vault_ids, []);
      pass('beta.deployments.update null-clears nullable/full-replacement axes');

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
      assert.equal(run.error, null, `unexpected launch error: ${JSON.stringify(run.error)}`);
      assert.ok(run.session_id, `a run with a valid initial event creates a Session: ${JSON.stringify(run)}`);
      pass('beta.deployments.run -> BetaManagedAgentsDeploymentRun');

      const gotRun = await client.beta.deploymentRuns.retrieve(run.id, { betas: BETAS });
      assert.equal(gotRun.id, run.id);
      const runs = await drain(client.beta.deploymentRuns.list({ deployment_id: dep.id, betas: BETAS }));
      assert.ok(runs.some((r) => r.id === run.id), 'the run is listed');
      pass(`beta.deploymentRuns.retrieve / list (${runs.length})`);

      // Local launch uses the Coordinator executable projection, not Control's
      // authoring history. Cause/effect D6: archiving withdraws that projection;
      // a later manual launch therefore reports environment_not_found, creates
      // no Session, appends the typed run, and does not auto-pause. The distinct
      // environment_archived outcome remains owned by launchers that receive an
      // authoritative lifecycle fact (covered by the DeploymentState F1/F3
      // decision-table unit test), never inferred from a withdrawn projection.
      const doomedEnvironment = await client.beta.environments.create({
        name: 'deployment-doomed', config: { type: 'cloud' }, betas: BETAS,
      });
      const doomed = await client.beta.deployments.create({
        agent: deploymentAgent.id, environment_id: doomedEnvironment.id, name: 'doomed',
        initial_events: [{ type: 'user.message', content: [{ type: 'text', text: 'go' }] }],
        betas: BETAS,
      });
      await client.beta.environments.archive(doomedEnvironment.id, { betas: BETAS });
      const failed = await client.beta.deployments.run(doomed.id, { betas: BETAS });
      assert.equal(failed.session_id, null, 'D6 failed creation has no Session');
      assert.equal(failed.error?.type, 'environment_not_found_error', JSON.stringify(failed));
      assert.match(failed.error?.message ?? '', /no longer exists/);
      assert.equal(failed.trigger_context.type, 'manual');
      const stillActive = await client.beta.deployments.retrieve(doomed.id, { betas: BETAS });
      assert.equal(stillActive.status, 'active', 'D6 only scheduled persistent failures auto-pause');
      assert.equal(stillActive.paused_reason, null, 'D6');

      const failures = await drain(client.beta.deploymentRuns.list({ has_error: true, betas: BETAS }));
      assert.deepEqual(failures.map((item) => item.id), [failed.id], 'D7 has_error=true');
      const successes = await drain(client.beta.deploymentRuns.list({ has_error: false, betas: BETAS }));
      assert.ok(successes.some((item) => item.id === run.id), 'D7 has_error=false includes success');
      assert.ok(successes.every((item) => item.error === null), 'D7 success partition is exact');
      const manual = await drain(client.beta.deploymentRuns.list({ trigger_type: 'manual', betas: BETAS }));
      assert.ok(manual.some((item) => item.id === run.id) && manual.some((item) => item.id === failed.id));
      const scheduled = await drain(client.beta.deploymentRuns.list({ trigger_type: 'schedule', betas: BETAS }));
      assert.deepEqual(scheduled, [], 'D7 no scheduled runs exist in this scenario');
      const inclusive = await drain(client.beta.deploymentRuns.list({
        'created_at[gte]': failed.created_at, 'created_at[lte]': failed.created_at, betas: BETAS,
      }));
      assert.ok(inclusive.some((item) => item.id === failed.id), 'D7 inclusive time bounds');
      const exclusive = await drain(client.beta.deploymentRuns.list({
        'created_at[gt]': failed.created_at, 'created_at[lt]': failed.created_at, betas: BETAS,
      }));
      assert.deepEqual(exclusive, [], 'D7 exclusive equal bounds');
      const runCountBeforeRejectedFilters = (await drain(client.beta.deploymentRuns.list({ betas: BETAS }))).length;
      for (const query of [
        'has_error=maybe',
        'trigger_type=timer',
        'created_at%5Bgt%5D=not-a-timestamp',
        'unsupported_filter=x',
        'limit=0',
        'limit=101',
      ]) {
        const response = await fetch(`${baseUrl}/v1/deployment_runs?${query}`);
        assert.equal(response.status, 400, `D8 ${query}`);
      }
      assert.equal(
        (await drain(client.beta.deploymentRuns.list({ betas: BETAS }))).length,
        runCountBeforeRejectedFilters,
        'D8 rejected query cannot mutate the append-only run store',
      );
      pass('deployment-run typed failure, terminal XOR, and list-filter decision table');

      const beforeInvalid = await drain(client.beta.deployments.list({ betas: BETAS }));
      await assert.rejects(
        client.beta.deployments.create({
          agent: deploymentAgent.id,
          environment_id: environment.id,
          name: 'invalid-event',
          initial_events: [{ type: 'user.interrupt' }],
          betas: BETAS,
        }),
      );
      const afterInvalid = await drain(client.beta.deployments.list({ betas: BETAS }));
      assert.equal(
        afterInvalid.length,
        beforeInvalid.length,
        'rejected initial-event union must not create a deployment',
      );
      pass('invalid deployment initial event fails before state mutation');

      const baseCreate = {
        agent: deploymentAgent.id, environment_id: environment.id, name: 'boundary',
        initial_events: [{ type: 'user.message', content: [{ type: 'text', text: 'go' }] }],
      };
      const repeatedEvents = Array.from({ length: 51 }, () => baseCreate.initial_events[0]);
      const metadata17 = Object.fromEntries(Array.from({ length: 17 }, (_, index) => [`k${index}`, 'v']));
      const rejectedWrites = [
        ['empty name', { ...baseCreate, name: '' }],
        ['no initial event', { ...baseCreate, initial_events: [] }],
        ['too many initial events', { ...baseCreate, initial_events: repeatedEvents }],
        ['outcome iterations zero', {
          ...baseCreate,
          initial_events: [{
            type: 'user.define_outcome', description: 'x', rubric: { type: 'text', content: 'x' }, max_iterations: 0,
          }],
        }],
        ['outcome iterations over max', {
          ...baseCreate,
          initial_events: [{
            type: 'user.define_outcome', description: 'x', rubric: { type: 'text', content: 'x' }, max_iterations: 21,
          }],
        }],
        ['too many metadata pairs', { ...baseCreate, metadata: metadata17 }],
        ['metadata key too long', { ...baseCreate, metadata: { ['k'.repeat(65)]: 'v' } }],
        ['metadata value too long', { ...baseCreate, metadata: { k: 'v'.repeat(513) } }],
        ['too many resources', {
          ...baseCreate,
          resources: Array.from({ length: 501 }, (_, index) => ({ type: 'file', file_id: `file_${index}` })),
        }],
        ['too many vaults', {
          ...baseCreate, vault_ids: Array.from({ length: 51 }, (_, index) => `vlt_${index}`),
        }],
        ['unknown create field', { ...baseCreate, parallel_owner: true }],
      ];
      const countBeforeBoundaryRejects = (await drain(client.beta.deployments.list({ betas: BETAS }))).length;
      for (const [rule, body] of rejectedWrites) {
        const response = await fetch(`${baseUrl}/v1/deployments`, {
          method: 'POST',
          headers: { 'content-type': 'application/json', 'anthropic-beta': BETAS[0] },
          body: JSON.stringify(body),
        });
        assert.equal(response.status, 400, `D9 ${rule}`);
      }
      assert.equal(
        (await drain(client.beta.deployments.list({ betas: BETAS }))).length,
        countBeforeBoundaryRejects,
        'D9 rejected creates produce no aggregate',
      );
      await assert.rejects(client.beta.deployments.update(dep.id, {
        name: 'must-not-stick', metadata: metadata17, betas: BETAS,
      }));
      assert.notEqual(
        (await client.beta.deployments.retrieve(dep.id, { betas: BETAS })).name,
        'must-not-stick',
        'D9 rejected update is atomic',
      );
      pass('deployment admission boundaries reject atomically');

      const archived = await client.beta.deployments.archive(dep.id, { betas: BETAS });
      assert.ok(archived.archived_at, 'archived deployment carries archived_at');
      const defaultIds = (await drain(client.beta.deployments.list({ betas: BETAS }))).map((d) => d.id);
      assert.ok(!defaultIds.includes(dep.id), 'D10 archived deployments are excluded by default');
      const archivedIds = (await drain(client.beta.deployments.list({
        include_archived: true, betas: BETAS,
      }))).map((d) => d.id);
      assert.ok(archivedIds.includes(dep.id), 'D10 include_archived');
      const activeOnly = await drain(client.beta.deployments.list({ status: 'active', betas: BETAS }));
      assert.ok(activeOnly.every((item) => item.status === 'active' && item.archived_at === null));
      const agentMiss = await drain(client.beta.deployments.list({ agent_id: 'agent_missing', betas: BETAS }));
      assert.deepEqual(agentMiss, [], 'D10 agent filter');
      const inclusiveDeployments = await drain(client.beta.deployments.list({
        'created_at[gte]': doomed.created_at, 'created_at[lte]': doomed.created_at, betas: BETAS,
      }));
      assert.ok(inclusiveDeployments.some((item) => item.id === doomed.id), 'D10 inclusive time bounds');
      const incompatibleFilters = await fetch(
        `${baseUrl}/v1/deployments?include_archived=true&status=active`,
      );
      assert.equal(incompatibleFilters.status, 400, 'D10 incompatible filters reject');
      assert.equal((await fetch(`${baseUrl}/v1/deployments?limit=101`)).status, 400, 'D10 max page size');
      pass('beta.deployments.archive and typed list-filter decision table');
    });

    console.log('E2E PASS: the deployments + deployment-runs families round-trip through the official @anthropic-ai/sdk.');
    process.exitCode = 0;
  } catch (err) {
    console.error('E2E FAIL:', err);
    process.exitCode = 1;
  }
}

main();
