// A richer tool state machine (coverage): a per-key machine whose transitions gate
// on the tool RESULT (`when: success` / `when: {status, content}`) rather than just
// the call args, plus a `key` template + normalizer. The model calls `glob` twice
// with the same pattern: unlike the basic statemachine e2e (where the second call
// is an out-of-order VIOLATION), here both calls advance — s0 → s1 on a success
// result, then s1 → s2 on a matching result. So the second glob is NOT denied; it
// succeeds. This exercises the result matchers (status + content) and the key
// template that the basic machine never touches.
//
// Run: (from e2e/)  node managed_statemachine_rich_e2e.mjs

import assert from 'node:assert/strict';
import Anthropic from '@anthropic-ai/sdk';
import { spawnServer, stopServer, waitForPort, pass } from './harness.mjs';

const PORT = Number(process.env.E2E_PORT ?? 38188);
const BASE = `http://127.0.0.1:${PORT}`;
const BETAS = ['managed-agents-2026-04-01'];
const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: BASE });

async function main() {
  const { server } = spawnServer('statemachine-rich', PORT);
  await waitForPort(PORT);
  try {
    const session = await client.beta.sessions.create({ agent: 'assistant', environment_id: 'env_local', betas: BETAS });
    await client.beta.sessions.events.send(session.id, {
      events: [{ type: 'user.message', content: [{ type: 'text', text: 'walk the keyed machine' }] }],
      betas: BETAS,
    });
    const events = [];
    for await (const ev of client.beta.sessions.events.list(session.id, { betas: BETAS })) events.push(ev);

    const toolUses = events.filter((e) => e.type === 'agent.tool_use');
    assert.equal(toolUses.length, 2, 'the model made two glob calls');
    assert.ok(toolUses.every((t) => t.name === 'glob'), 'both calls are the machine-gated glob tool');
    pass('two glob calls issued through the result-gated machine');

    // Both calls ADVANCED (result-gated transitions): neither result is a machine
    // violation — the second glob succeeded from s1 rather than being denied.
    const results = events.filter((e) => e.type === 'agent.tool_result');
    const anyViolation = results.some((r) => JSON.stringify(r.content ?? '').includes('violation') || JSON.stringify(r.content ?? '').includes('only allowed'));
    assert.ok(!anyViolation, 'no violation — both result-gated transitions advanced (s0→s1→s2)');
    pass('result matchers advanced the machine on both calls (when: success / {status, content})');

    const lastIdle = [...events].reverse().find((e) => e.type === 'session.status_idle');
    assert.equal(lastIdle.stop_reason.type, 'end_turn', 'run completed after reaching the terminal state');
    pass('run completed end_turn after reaching the terminal state s2');

    console.log('E2E PASS: result-gated + keyed tool state machine (result matchers + key template).');
  } finally {
    await stopServer(server);
  }
}

main().catch((err) => {
  console.error('E2E FAIL:', err);
  process.exitCode = 1;
});
