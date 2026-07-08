// Scheduled action / scheduled wake end-to-end (slice E, ADR-0020) via the
// Anthropic TS SDK.
//
// The `schedule` server's tool gate defers every tool call as a committed
// `ScheduledAction` instead of running it inline or parking for a human. Under
// AWAKEN_INGRESS=durable the dispatch worker's scheduled-action loop performs each
// deferred call out of band, so a run that would otherwise require a confirmation
// (see managed_hitl / managed_restart, which PARK on the same probe tools) here
// completes autonomously: write → read → done, no `requires_action`.
//
// This is a durable-only capability: a direct ingress has no worker to perform the
// scheduled action, so the run would stay parked.
//
// Run: (from e2e/)  node managed_scheduled_e2e.mjs

import assert from 'node:assert/strict';
import fs from 'node:fs';
import Anthropic from '@anthropic-ai/sdk';
import { spawnServer, stopServer, waitForPort, pass, startUpstream, realServerEnv } from './harness.mjs';

const PORT = Number(process.env.E2E_PORT ?? 38178);
const BASE = `http://127.0.0.1:${PORT}`;
const BETAS = ['managed-agents-2026-04-01'];
const STORE_DIR = `/tmp/awaken-scheduled-e2e-${process.pid}`;
const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: BASE });

const listEvents = async (sessionId) => {
  const events = [];
  for await (const ev of client.beta.sessions.events.list(sessionId, { betas: BETAS })) events.push(ev);
  return events;
};

async function main() {
  fs.rmSync(STORE_DIR, { recursive: true, force: true });
  fs.mkdirSync(STORE_DIR, { recursive: true });
  const upstream = await startUpstream('probe');
  const srv = spawnServer('schedule', PORT, { AWAKEN_STORAGE_DIR: STORE_DIR, AWAKEN_INGRESS: 'durable', ...realServerEnv('probe', upstream, { mode: 'schedule' }) });
  await waitForPort(PORT);
  try {
    const session = await client.beta.sessions.create({ agent: 'assistant', environment_id: 'env_local', betas: BETAS });
    await client.beta.sessions.events.send(session.id, {
      events: [{ type: 'user.message', content: [{ type: 'text', text: 'SCHEDULE-ME' }] }],
      betas: BETAS,
    });
    const events = await listEvents(session.id);

    // The run completed autonomously — no human confirmation was needed, because
    // the durable worker performed the scheduled tool calls out of band.
    const idle = [...events].reverse().find((e) => e.type === 'session.status_idle');
    assert.equal(idle.stop_reason.type, 'end_turn', 'scheduled run completed without requiring_action');
    assert.ok(
      !events.some((e) => e.type === 'session.status_idle' && e.stop_reason.type === 'requires_action'),
      'no confirmation was ever requested — the actions were scheduled, not suspended',
    );
    pass('run completed autonomously — scheduled actions performed by the durable worker');

    // The deferred tool calls actually ran: the probe wrote then read `probe.txt`,
    // so a tool result reflecting the input is present.
    const results = events.filter((e) => e.type === 'agent.tool_result');
    assert.ok(results.length >= 1, 'the scheduled tool calls were performed (tool results present)');
    assert.ok(
      JSON.stringify(results.map((r) => r.content)).includes('SCHEDULE-ME'),
      'the scheduled write/read round-tripped the input (the action really ran)',
    );
    pass(`${results.length} scheduled tool call(s) performed out of band and committed`);

    const done = events.filter((e) => e.type === 'agent.message');
    assert.ok(JSON.stringify(done.map((m) => m.content)).includes('done'), 'run reached its natural end');
    pass('scheduled-action run reached its natural end (ADR-0020)');

    console.log('E2E PASS: scheduled action performed by the durable worker (ADR-0020).');
  } finally {
    await stopServer(srv.server);
    upstream.close();
    fs.rmSync(STORE_DIR, { recursive: true, force: true });
  }
}

main().catch((err) => {
  console.error('E2E FAIL:', err);
  process.exitCode = 1;
});
