// Scheduled deployments (scheduled-deployments doc): a deployment created with a
// cron `schedule` must store + echo the {type,expression,timezone} verbatim, reject
// an unparseable cron at write time (400, not silently stored), and keep the schedule
// through a pause/unpause cycle. `management_deployments_e2e.mjs` covers CRUD,
// manual runs, and schedule replacement; this suite owns occurrence projection,
// production timer execution, and schedule-specific lifecycle alternatives.
//
// Causal graph: typed cron + IANA timezone -> wall-clock scheduler -> future UTC
// occurrences; lifecycle gates execution/projection independently.
//
// Decision table:
// | rule | cron/tz | lifecycle | observable behavior |
// |---|---|---|---|
// | S1 | valid | active | five ordered upcoming occurrences |
// | S2 | valid | paused | no fire, but preview remains |
// | S3 | valid | archived | preview clears |
// | S4 | invalid expression/timezone | any | 400 before persistence |
// | S5 | due + withdrawn Environment projection | active | not-found run then exact auto-pause |
// | S6 | primary Agent archived | any | Deployment archives in same operation; no run |
// | S7 | Deployment archived | terminal | mutation/manual run reject |
// | S8 | archived referenced subagent | active schedule | failed run + exact Agent error + auto-pause |
//
// Causes: cron/timezone validity, Agent/Environment lifecycle, exact due instant,
// and Deployment active/paused/archived state.
// Constraints: accepted schedules bind an existing active Agent and Environment;
// previews remain exact while the production timer alone applies execution jitter.
// Effects: ordered previews, suppressed or terminal schedules, typed failed runs,
// auto-pause, synchronous primary-Agent archive cascade, and atomic rejection.
// Decision rules: S1-S8 above cover the schedule-owned alternatives without
// repeating the general Deployment CRUD state machine.
//
// Run: (from e2e/)  node management_deployment_schedule_e2e.mjs

import assert from 'node:assert/strict';
import Anthropic from '@anthropic-ai/sdk';
import { withScenarioServer, pass } from './harness.mjs';

const PORT = Number(process.env.E2E_PORT ?? 38433);
const BETAS = ['managed-agents-2026-04-01'];
const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

async function drain(pagePromise) {
  const items = [];
  for await (const item of pagePromise) items.push(item);
  return items;
}

async function status(fn) {
  try { await fn(); return 200; } catch (e) { return e?.status ?? -1; }
}

async function main() {
  await withScenarioServer('management', 'mcp', PORT, async (baseUrl) => {
    const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: baseUrl });
    const scheduledAgent = await client.beta.agents.create({
      name: 'scheduled-agent', model: 'claude-opus-4-8', betas: BETAS,
    });
    const environment = await client.beta.environments.create({
      name: 'scheduled-environment', config: { type: 'cloud' }, betas: BETAS,
    });

    // Valid cron: "every Friday at 20:00 America/New_York".
    const schedule = { type: 'cron', expression: '0 20 * * 5', timezone: 'America/New_York' };
    const dep = await client.beta.deployments.create({
      agent: scheduledAgent.id,
      environment_id: environment.id,
      name: 'weekly-report',
      initial_events: [{ type: 'user.message', content: [{ type: 'text', text: 'go' }] }],
      schedule,
      betas: BETAS,
    });
    assert.equal(dep.status, 'active');
    assert.ok(dep.schedule, 'created deployment echoes a schedule object');
    assert.equal(dep.schedule.expression, '0 20 * * 5', `expression echoed: ${JSON.stringify(dep.schedule)}`);
    assert.equal(dep.schedule.timezone, 'America/New_York', 'timezone echoed');
    assert.equal(dep.schedule.upcoming_runs_at?.length, 5, 'S1 future occurrences are computed');
    assert.ok(
      dep.schedule.upcoming_runs_at.every((value, index, values) => index === 0 || values[index - 1] < value),
      `S1 occurrences are strictly ordered: ${JSON.stringify(dep.schedule)}`,
    );
    const firstLocal = Object.fromEntries(
      new Intl.DateTimeFormat('en-US', {
        timeZone: 'America/New_York', weekday: 'short', hour: '2-digit', hourCycle: 'h23',
      }).formatToParts(new Date(dep.schedule.upcoming_runs_at[0])).map((part) => [part.type, part.value]),
    );
    assert.equal(firstLocal.weekday, 'Fri', 'S1 cron weekday is evaluated in declared timezone');
    assert.equal(firstLocal.hour, '20', 'S1 cron hour is evaluated in declared timezone');
    pass('create with cron schedule -> active, expression/timezone echoed verbatim');

    // Schedule survives a retrieve (persisted, not just request-echoed).
    const got = await client.beta.deployments.retrieve(dep.id, { betas: BETAS });
    assert.equal(got.schedule?.expression, '0 20 * * 5', 'schedule persists on retrieve');
    pass('schedule persists across retrieve');

    // Pause suspends the schedule (manual reason); unpause restores it, schedule intact.
    const paused = await client.beta.deployments.pause(dep.id, { betas: BETAS });
    assert.equal(paused.status, 'paused');
    assert.equal(paused.paused_reason?.type, 'manual', `paused_reason: ${JSON.stringify(paused.paused_reason)}`);
    assert.equal(paused.schedule?.expression, '0 20 * * 5', 'schedule retained while paused');
    assert.equal(paused.schedule?.upcoming_runs_at?.length, 5, 'S2 paused preview remains');
    const resumed = await client.beta.deployments.unpause(dep.id, { betas: BETAS });
    assert.equal(resumed.status, 'active');
    assert.equal(resumed.paused_reason, null, 'paused_reason cleared on unpause');
    assert.equal(resumed.schedule?.expression, '0 20 * * 5', 'schedule intact after unpause');
    pass('pause/unpause preserves the schedule');

    // Garbage cron expression -> 400 (rejected at write time, not stored).
    const badExpr = await status(() => client.beta.deployments.create({
      agent: scheduledAgent.id, environment_id: environment.id, name: 'bad',
      initial_events: [{ type: 'user.message', content: [{ type: 'text', text: 'x' }] }],
      schedule: { type: 'cron', expression: 'not a cron', timezone: 'UTC' }, betas: BETAS,
    }));
    assert.equal(badExpr, 400, `unparseable cron should be 400, got ${badExpr}`);
    pass('unparseable cron expression -> 400');

    // Schedule object without an `expression` -> 400.
    const noExpr = await status(() => client.beta.deployments.create({
      agent: scheduledAgent.id, environment_id: environment.id, name: 'noexpr',
      initial_events: [{ type: 'user.message', content: [{ type: 'text', text: 'x' }] }],
      schedule: { type: 'cron', timezone: 'UTC' }, betas: BETAS,
    }));
    assert.equal(noExpr, 400, `schedule without expression should be 400, got ${noExpr}`);
    pass('schedule missing expression -> 400');

    const badTimezone = await status(() => client.beta.deployments.create({
      agent: scheduledAgent.id, environment_id: environment.id, name: 'bad-timezone',
      initial_events: [{ type: 'user.message', content: [{ type: 'text', text: 'x' }] }],
      schedule: { type: 'cron', expression: '0 9 * * *', timezone: 'Mars/Olympus' }, betas: BETAS,
    }));
    assert.equal(badTimezone, 400, `unknown IANA timezone should be 400, got ${badTimezone}`);
    pass('unknown IANA timezone -> 400');

    // S6/S7 use the real shared Agent repository + DeploymentState assembled by
    // the management composition. Archiving the primary Agent cascades before
    // the Agent request returns; no scheduler race may create a run afterward.
    const primary = await client.beta.agents.create({
      name: 'scheduled-primary', model: 'claude-opus-4-8', betas: BETAS,
    });
    const primaryDeployment = await client.beta.deployments.create({
      agent: primary.id,
      environment_id: environment.id,
      name: 'primary-archive-cascade',
      initial_events: [{ type: 'user.message', content: [{ type: 'text', text: 'go' }] }],
      schedule: { type: 'cron', expression: '* * * * *', timezone: 'UTC' },
      betas: BETAS,
    });
    await client.beta.agents.archive(primary.id, { betas: BETAS });
    const cascaded = await client.beta.deployments.retrieve(primaryDeployment.id, { betas: BETAS });
    assert.ok(cascaded.archived_at, 'S6 primary Agent archive cascades synchronously');
    const cascadeRuns = await drain(client.beta.deploymentRuns.list({
      deployment_id: primaryDeployment.id, betas: BETAS,
    }));
    assert.equal(cascadeRuns.length, 0, 'S6 cascade creates no deployment run');
    await assert.rejects(
      () => client.beta.deployments.run(primaryDeployment.id, { betas: BETAS }),
      (error) => error.status === 409,
      'S7 archived Deployment cannot run manually',
    );
    pass('primary Agent archive cascades to terminal Deployment without a run');

    const archived = await client.beta.deployments.archive(dep.id, { betas: BETAS });
    assert.deepEqual(archived.schedule?.upcoming_runs_at ?? [], [], 'S3 archived schedule has no future fires');

    // S5 drives the production 15-second scheduler rather than calling an
    // internal clock seam. The exact Environment projection becomes unavailable
    // before the next minute boundary; the local launcher cannot infer an archive
    // tombstone, so the run records not-found and atomically pauses future fires
    // with the same discriminator (M19/T133).
    const scheduledEnvironment = await client.beta.environments.create({
      name: 'scheduled-failure', config: { type: 'cloud' }, betas: BETAS,
    });
    const failingSchedule = await client.beta.deployments.create({
      agent: scheduledAgent.id, environment_id: scheduledEnvironment.id, name: 'scheduled-failure',
      initial_events: [{ type: 'user.message', content: [{ type: 'text', text: 'go' }] }],
      schedule: { type: 'cron', expression: '* * * * *', timezone: 'UTC' }, betas: BETAS,
    });
    const archivedSubagent = await client.beta.agents.create({
      name: 'scheduled-archived-subagent', model: 'claude-opus-4-8', betas: BETAS,
    });
    const coordinatingAgent = await client.beta.agents.create({
      name: 'scheduled-coordinator',
      model: 'claude-opus-4-8',
      multiagent: { type: 'coordinator', agents: [archivedSubagent.id] },
      betas: BETAS,
    });
    const archivedSubagentSchedule = await client.beta.deployments.create({
      agent: coordinatingAgent.id,
      environment_id: environment.id,
      name: 'scheduled-archived-subagent',
      initial_events: [{ type: 'user.message', content: [{ type: 'text', text: 'go' }] }],
      schedule: { type: 'cron', expression: '* * * * *', timezone: 'UTC' },
      betas: BETAS,
    });
    await client.beta.environments.archive(scheduledEnvironment.id, { betas: BETAS });
    await client.beta.agents.archive(archivedSubagent.id, { betas: BETAS });
    const deadline = Date.now() + 80_000;
    let failedRun;
    let archivedSubagentRun;
    while (Date.now() < deadline) {
      const runs = await drain(client.beta.deploymentRuns.list({
        deployment_id: failingSchedule.id, trigger_type: 'schedule', betas: BETAS,
      }));
      failedRun = runs.find((run) => run.error?.type === 'environment_not_found_error');
      const archivedRuns = await drain(client.beta.deploymentRuns.list({
        deployment_id: archivedSubagentSchedule.id, trigger_type: 'schedule', betas: BETAS,
      }));
      archivedSubagentRun = archivedRuns.find((run) => run.error?.type === 'agent_archived_error');
      if (failedRun && archivedSubagentRun) break;
      await sleep(1_000);
    }
    assert.ok(failedRun, 'S5 production scheduler records the withdrawn-Environment failure');
    assert.equal(failedRun.session_id, null, 'S5 terminal XOR');
    const autoPaused = await client.beta.deployments.retrieve(failingSchedule.id, { betas: BETAS });
    assert.equal(autoPaused.status, 'paused', 'S5 future scheduled fires stop');
    assert.equal(autoPaused.paused_reason?.type, 'error', 'S5 error pause');
    assert.equal(
      autoPaused.paused_reason?.error?.type,
      failedRun.error.type,
      'S5 paused reason exactly matches the failed run error',
    );
    assert.ok(archivedSubagentRun, 'S8 scheduled run records the archived subagent failure');
    assert.equal(archivedSubagentRun.session_id, null, 'S8 terminal XOR');
    assert.match(
      archivedSubagentRun.error?.message ?? '',
      new RegExp(archivedSubagent.id),
      'S8 exact referenced subagent explains the failure',
    );
    const subagentPaused = await client.beta.deployments.retrieve(archivedSubagentSchedule.id, {
      betas: BETAS,
    });
    assert.equal(subagentPaused.status, 'paused', 'S8 future scheduled fires stop');
    assert.equal(subagentPaused.paused_reason?.type, 'error', 'S8 error pause');
    assert.equal(
      subagentPaused.paused_reason?.error?.type,
      'agent_archived_error',
      'S8 paused reason mirrors the failed run',
    );
    pass('scheduled persistent launch failure records run and auto-pauses exactly');

    console.log('E2E PASS: scheduled deployments (timezone-aware preview, lifecycle behavior, write-time validation).');
  });
}

main().catch((err) => {
  console.error('E2E FAIL:', err);
  process.exit(1);
});
