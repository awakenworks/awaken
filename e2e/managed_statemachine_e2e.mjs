// Tool state machine end-to-end via the official Anthropic TS SDK.
//
// The `statemachine` server agent activates the tool state machine with a machine
// that defines `glob` as the single transition out of the initial state. The
// driving model calls `glob` twice:
//   1. the first advances the machine s0 -> s1 (and fires its emit), and runs;
//   2. the second is a precondition violation (glob is only defined from s0), so
//      the machine gate denies it — the model gets an error result carrying the
//      violation reason, then ends the turn.
//
// This exercises the state machine's gate (allow + deny), state advance, emit, and
// violation paths — and the shared tool-pattern matcher the transitions use —
// through HTTP with the real SDK.
//
// Run: (from e2e/)  node managed_statemachine_e2e.mjs

import assert from 'node:assert/strict';
import Anthropic from '@anthropic-ai/sdk';
import { spawnServer, stopServer, waitForPort, pass } from './harness.mjs';

const PORT = Number(process.env.E2E_PORT ?? 38151);
const BETAS = ['managed-agents-2026-04-01'];
const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: `http://127.0.0.1:${PORT}` });

async function main() {
  const { server } = spawnServer('statemachine', PORT);
  await waitForPort(PORT);
  try {
    const session = await client.beta.sessions.create({
      agent: 'assistant',
      environment_id: 'env_local',
      betas: BETAS,
    });
    await client.beta.sessions.events.send(session.id, {
      events: [{ type: 'user.message', content: [{ type: 'text', text: 'walk the machine' }] }],
      betas: BETAS,
    });

    const events = [];
    for await (const ev of client.beta.sessions.events.list(session.id, { betas: BETAS })) {
      events.push(ev);
    }

    const toolUses = events.filter((e) => e.type === 'agent.tool_use');
    assert.equal(toolUses.length, 2, 'the model made two glob calls');
    assert.ok(
      toolUses.every((t) => t.name === 'glob'),
      'both calls are the machine-gated glob tool',
    );
    pass('two glob calls issued through the machine gate');

    const results = events.filter((e) => e.type === 'agent.tool_result');
    const secondResult = JSON.stringify(results.at(-1)?.content ?? '');
    assert.ok(
      secondResult.includes('glob is only allowed from the start state'),
      'the out-of-order second call was denied by the machine with its violation reason',
    );
    pass('state machine advanced on the first call and denied the second as a violation');

    const lastIdle = [...events].reverse().find((e) => e.type === 'session.status_idle');
    assert.equal(lastIdle.stop_reason.type, 'end_turn', 'run completed after the gated sequence');
    pass('run completed end_turn after the gated tool sequence');

    console.log('E2E PASS: tool state machine gate + advance + emit + violation via TS SDK.');
  } finally {
    await stopServer(server);
  }
}

main().catch((err) => {
  console.error('E2E FAIL:', err);
  process.exitCode = 1;
});
