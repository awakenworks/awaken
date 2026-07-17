// Scheduled deployments (scheduled-deployments doc): a deployment created with a
// cron `schedule` must store + echo the {type,expression,timezone} verbatim, reject
// an unparseable cron at write time (400, not silently stored), and keep the schedule
// through a pause/unpause cycle. `management_deployments_e2e.mjs` covers the CRUD +
// manual-run lifecycle but never sends a `schedule` — the cron half of the deployment
// contract is otherwise untested.
//
// Design: equivalence partitioning on the cron expression (valid 5-field vs garbage
// vs missing); state-transition on pause→unpause preserving the schedule. The awaken
// server validates the expression via its dependency-free cron evaluator at create.
//
// Run: (from e2e/)  node management_deployment_schedule_e2e.mjs

import assert from 'node:assert/strict';
import Anthropic from '@anthropic-ai/sdk';
import { withScenarioServer, pass } from './harness.mjs';

const PORT = Number(process.env.E2E_PORT ?? 38433);
const BETAS = ['managed-agents-2026-04-01'];

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

    console.log('E2E PASS: scheduled deployments (cron schedule echo/persist, pause-retains, write-time cron validation).');
  });
}

main().catch((err) => {
  console.error('E2E FAIL:', err);
  process.exit(1);
});
