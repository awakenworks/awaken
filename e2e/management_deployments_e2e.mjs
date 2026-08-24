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
import { withScenarioServer, pass, waitForValue } from './harness.mjs';

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
 * | D11  | sole define_outcome | absent     | absent          | launch Session with the exact initial outcome event |
 * | D12  | valid user | create/update/null budget | absent    | each new Session freezes only the current cap |
 * | D13  | valid user | low budget          | absent          | linked Session reaches budget_reached |
 * | D14  | valid user | create bucket exhausted | absent      | one typed rate-limit run; Deployment remains active |
 *
 * Causes: authoritative Agent/Environment state, create/update inputs,
 * lifecycle operation, trigger result, and list-filter values.
 * Constraints: every accepted Deployment binds one existing active Agent
 * version and one Workspace-owned Environment before mutation.
 * Effects: typed Deployment/Run projections, one ordinary Session on a valid
 * trigger, atomic rejection, terminal archive, and exact list partitions.
 * Decision rules: D1-D14 above cover the public CRUD, manual-trigger, initial
 * outcome, budget, rate-limit, failure, boundary, and filter combinations owned
 * by this official SDK suite. Hosted Anthropic quota, billing, and synthesis
 * quality remain external acceptance evidence; this deterministic suite proves
 * only Awaken's compatible wire and state-machine effects.
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
      const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: baseUrl, maxRetries: 0 });
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
      const pausedRun = await client.beta.deployments.run(dep.id, { betas: BETAS });
      assert.equal(pausedRun.error, null, 'D4 paused manual run still launches');
      assert.ok(pausedRun.session_id, 'D4 paused manual run links a Session');
      assert.equal(pausedRun.trigger_context.type, 'manual');
      const remainsPaused = await client.beta.deployments.retrieve(dep.id, { betas: BETAS });
      assert.equal(remainsPaused.status, 'paused', 'manual run does not unpause the schedule');
      assert.equal(remainsPaused.paused_reason?.type, 'manual');
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

      const budgetedDeployment = await client.beta.deployments.create({
        agent: deploymentAgent.id,
        environment_id: environment.id,
        name: 'per-run-budget',
        initial_events: [{ type: 'user.message', content: [{ type: 'text', text: 'add 2 3' }] }],
        budget: { type: 'limit', max_list_cost: { amount: '2000', currency: 'USD' } },
        betas: BETAS,
      });
      assert.equal(budgetedDeployment.budget?.max_list_cost.amount, '2000', 'D12 create projection');
      const firstBudgetRun = await client.beta.deployments.run(budgetedDeployment.id, { betas: BETAS });
      assert.equal(firstBudgetRun.error, null, 'D12 first run');
      const firstBudgetSession = await client.beta.sessions.retrieve(firstBudgetRun.session_id, {
        betas: BETAS,
      });
      assert.equal(firstBudgetSession.budget?.max_list_cost.amount, '2000', 'D12 first cap');

      const updatedBudget = await client.beta.deployments.update(budgetedDeployment.id, {
        budget: { type: 'limit', max_list_cost: { amount: '500', currency: 'USD' } },
        betas: BETAS,
      });
      assert.equal(updatedBudget.budget?.max_list_cost.amount, '500', 'D12 update projection');
      const secondBudgetRun = await client.beta.deployments.run(budgetedDeployment.id, { betas: BETAS });
      const secondBudgetSession = await client.beta.sessions.retrieve(secondBudgetRun.session_id, {
        betas: BETAS,
      });
      assert.equal(secondBudgetSession.budget?.max_list_cost.amount, '500', 'D12 future cap');
      assert.equal(
        (await client.beta.sessions.retrieve(firstBudgetRun.session_id, { betas: BETAS }))
          .budget?.max_list_cost.amount,
        '2000',
        'D12 prior Session cap is unchanged',
      );

      const clearedBudget = await client.beta.deployments.update(budgetedDeployment.id, {
        budget: null,
        betas: BETAS,
      });
      assert.equal(clearedBudget.budget, null, 'D12 null clears Deployment cap');
      const thirdBudgetRun = await client.beta.deployments.run(budgetedDeployment.id, { betas: BETAS });
      const thirdBudgetSession = await client.beta.sessions.retrieve(thirdBudgetRun.session_id, {
        betas: BETAS,
      });
      assert.equal(thirdBudgetSession.budget, null, 'D12 null applies to later Session only');
      pass('Deployment budget create/update/null freezes independently onto each Session');

      const lowBudgetDeployment = await client.beta.deployments.create({
        agent: deploymentAgent.id,
        environment_id: environment.id,
        name: 'budget-reached',
        initial_events: [{ type: 'user.message', content: [{ type: 'text', text: 'add 2 3' }] }],
        budget: { type: 'limit', max_list_cost: { amount: '1', currency: 'USD' } },
        betas: BETAS,
      });
      const lowBudgetRun = await client.beta.deployments.run(lowBudgetDeployment.id, { betas: BETAS });
      const requiresAction = await waitForValue(
        () => drain(client.beta.sessions.events.list(lowBudgetRun.session_id, { betas: BETAS })),
        (events) => events.some((event) =>
          event.type === 'session.status_idle' && event.stop_reason.type === 'requires_action'),
        'D13 Deployment Session reaches its client-tool boundary',
      );
      const toolUse = requiresAction.find((event) => event.type === 'agent.mcp_tool_use');
      assert.ok(toolUse?.id, 'D13 public tool-use id');
      await client.beta.sessions.events.send(lowBudgetRun.session_id, {
        events: [{ type: 'user.tool_confirmation', tool_use_id: toolUse.id, result: 'allow' }],
        betas: BETAS,
      });
      await waitForValue(
        () => drain(client.beta.sessions.events.list(lowBudgetRun.session_id, { betas: BETAS })),
        (events) => events.some((event) =>
          event.type === 'session.status_idle' && event.stop_reason.type === 'budget_reached'),
        'D13 copied Deployment budget reaches the canonical Session gate',
      );
      pass('a Deployment-copied budget reaches the canonical budget_reached Session outcome');

      // D11 is intentionally sequenced after the Runtime-dependent budget rule:
      // a DefineOutcome may keep its ordinary grader Run live, while this rule
      // owns only successful Deployment admission and exact initial-event commit.
      const outcomeDeployment = await client.beta.deployments.create({
        agent: deploymentAgent.id,
        environment_id: environment.id,
        name: 'outcome-only',
        initial_events: [{
          type: 'user.define_outcome',
          description: 'Produce a verified report',
          rubric: { type: 'text', content: 'The report contains VERIFIED.' },
          max_iterations: 3,
        }],
        betas: BETAS,
      });
      const outcomeRun = await client.beta.deployments.run(outcomeDeployment.id, { betas: BETAS });
      assert.equal(outcomeRun.error, null, 'D11 valid sole outcome launches');
      assert.ok(outcomeRun.session_id, 'D11 linked Session');
      const outcomeEvents = await drain(client.beta.sessions.events.list(outcomeRun.session_id, {
        betas: BETAS,
      }));
      const defined = outcomeEvents.find((event) => event.type === 'user.define_outcome');
      assert.equal(defined?.description, 'Produce a verified report', 'D11 exact initial event');
      assert.equal(defined?.max_iterations, 3, 'D11 exact outcome bound');
      pass('sole user.define_outcome initial event launches through the official SDK');

      const rateLimitTarget = await client.beta.deployments.create({
        agent: deploymentAgent.id,
        environment_id: environment.id,
        name: 'rate-limit-target',
        initial_events: [{ type: 'user.message', content: [{ type: 'text', text: 'go' }] }],
        betas: BETAS,
      });
      let createRateLimited = false;
      for (let index = 0; index < 450; index += 1) {
        try {
          await client.beta.deployments.create({
            agent: deploymentAgent.id,
            environment_id: environment.id,
            name: `rate-drain-${index}`,
            initial_events: [{ type: 'user.message', content: [{ type: 'text', text: 'go' }] }],
            betas: BETAS,
          });
        } catch (error) {
          assert.equal(error?.status, 429, `D14 official SDK surfaces HTTP rate limit: ${error}`);
          createRateLimited = true;
          break;
        }
      }
      assert.ok(createRateLimited, 'D14 deterministic local create bucket is exhausted');
      const rateLimitedRun = await client.beta.deployments.run(rateLimitTarget.id, { betas: BETAS });
      assert.equal(rateLimitedRun.session_id, null, 'D14 failed launch creates no Session');
      assert.equal(rateLimitedRun.error?.type, 'session_rate_limited_error', 'D14 typed SDK union');
      assert.equal(rateLimitedRun.trigger_context.type, 'manual');
      const rateLimitRuns = await drain(client.beta.deploymentRuns.list({
        deployment_id: rateLimitTarget.id,
        betas: BETAS,
      }));
      assert.deepEqual(rateLimitRuns.map((item) => item.id), [rateLimitedRun.id], 'D14 no retry row');
      const rateLimitDeployment = await client.beta.deployments.retrieve(rateLimitTarget.id, {
        betas: BETAS,
      });
      assert.equal(rateLimitDeployment.status, 'active', 'D14 transient rate limit does not pause');
      assert.equal(rateLimitDeployment.paused_reason, null, 'D14');
      pass('official SDK sees session_rate_limited_error without retry or auto-pause');
    });

    console.log('E2E PASS: the deployments + deployment-runs families round-trip through the official @anthropic-ai/sdk.');
    process.exitCode = 0;
  } catch (err) {
    console.error('E2E FAIL:', err);
    process.exitCode = 1;
  }
}

main();
