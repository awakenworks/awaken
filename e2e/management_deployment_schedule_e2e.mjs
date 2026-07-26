// Scheduled deployments (scheduled-deployments doc): a deployment created with a
// cron `schedule` must store + echo the {type,expression,timezone} verbatim, reject
// an unparseable cron at write time (400, not silently stored), and keep the schedule
// through a pause/unpause cycle. `management_deployments_e2e.mjs` covers the CRUD +
// manual-run lifecycle but never sends a `schedule` — the cron half of the deployment
// contract is otherwise untested.
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
// | S5 | due + archived Environment | active | failed run then exact auto-pause |
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

    // Valid cron: "every Friday at 20:00 America/New_York".
    const schedule = { type: 'cron', expression: '0 20 * * 5', timezone: 'America/New_York' };
    const dep = await client.beta.deployments.create({
      agent: 'agent_sched',
      environment_id: 'env_1',
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
      agent: 'agent_sched', environment_id: 'env_1', name: 'bad',
      initial_events: [{ type: 'user.message', content: [{ type: 'text', text: 'x' }] }],
      schedule: { type: 'cron', expression: 'not a cron', timezone: 'UTC' }, betas: BETAS,
    }));
    assert.equal(badExpr, 400, `unparseable cron should be 400, got ${badExpr}`);
    pass('unparseable cron expression -> 400');

    // Schedule object without an `expression` -> 400.
    const noExpr = await status(() => client.beta.deployments.create({
      agent: 'agent_sched', environment_id: 'env_1', name: 'noexpr',
      initial_events: [{ type: 'user.message', content: [{ type: 'text', text: 'x' }] }],
      schedule: { type: 'cron', timezone: 'UTC' }, betas: BETAS,
    }));
    assert.equal(noExpr, 400, `schedule without expression should be 400, got ${noExpr}`);
    pass('schedule missing expression -> 400');

    const badTimezone = await status(() => client.beta.deployments.create({
      agent: 'agent_sched', environment_id: 'env_1', name: 'bad-timezone',
      initial_events: [{ type: 'user.message', content: [{ type: 'text', text: 'x' }] }],
      schedule: { type: 'cron', expression: '0 9 * * *', timezone: 'Mars/Olympus' }, betas: BETAS,
    }));
    assert.equal(badTimezone, 400, `unknown IANA timezone should be 400, got ${badTimezone}`);
    pass('unknown IANA timezone -> 400');

    const archived = await client.beta.deployments.archive(dep.id, { betas: BETAS });
    assert.deepEqual(archived.schedule?.upcoming_runs_at ?? [], [], 'S3 archived schedule has no future fires');

    // S5 drives the production 15-second scheduler rather than calling an
    // internal clock seam. The exact Environment becomes unavailable before the
    // next minute boundary; the scheduled run must record the typed failure and
    // atomically pause future fires with the same discriminator.
    const scheduledEnvironment = await client.beta.environments.create({
      name: 'scheduled-failure', config: { type: 'cloud' }, betas: BETAS,
    });
    const failingSchedule = await client.beta.deployments.create({
      agent: 'agent_sched', environment_id: scheduledEnvironment.id, name: 'scheduled-failure',
      initial_events: [{ type: 'user.message', content: [{ type: 'text', text: 'go' }] }],
      schedule: { type: 'cron', expression: '* * * * *', timezone: 'UTC' }, betas: BETAS,
    });
    await client.beta.environments.archive(scheduledEnvironment.id, { betas: BETAS });
    const deadline = Date.now() + 80_000;
    let failedRun;
    while (Date.now() < deadline) {
      const runs = await drain(client.beta.deploymentRuns.list({
        deployment_id: failingSchedule.id, trigger_type: 'schedule', betas: BETAS,
      }));
      failedRun = runs.find((run) => run.error?.type === 'environment_archived_error');
      if (failedRun) break;
      await sleep(1_000);
    }
    assert.ok(failedRun, 'S5 production scheduler records the archived-Environment failure');
    assert.equal(failedRun.session_id, null, 'S5 terminal XOR');
    const autoPaused = await client.beta.deployments.retrieve(failingSchedule.id, { betas: BETAS });
    assert.equal(autoPaused.status, 'paused', 'S5 future scheduled fires stop');
    assert.equal(autoPaused.paused_reason?.type, 'error', 'S5 error pause');
    assert.equal(
      autoPaused.paused_reason?.error?.type,
      failedRun.error.type,
      'S5 paused reason exactly matches the failed run error',
    );
    pass('scheduled persistent launch failure records run and auto-pauses exactly');

    console.log('E2E PASS: scheduled deployments (timezone-aware preview, lifecycle behavior, write-time validation).');
  });
}

main().catch((err) => {
  console.error('E2E FAIL:', err);
  process.exit(1);
});
