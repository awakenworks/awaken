// Brain–hand separation end-to-end (ADR-0044) through the served binary.
//
// The `remote-hand` server mode routes every run's tool calls to a HAND task
// serving the built-in tools over a framed in-process channel — not the run
// loop's in-process registry. The driving model calls `bash` to echo a fixed
// marker; the hand runs it out of the loop and its stdout round-trips back to the
// brain, which the model then echoes. Seeing the marker in the agent's reply
// proves the whole brain→(framed channel)→hand→brain path in the product binary.
//
// Run: (from e2e/)  npm install && node managed_remote_hand_e2e.mjs

import assert from 'node:assert/strict';
import Anthropic from '@anthropic-ai/sdk';
import { withServer } from './harness.mjs';

const PORT = Number(process.env.E2E_PORT ?? 38141);
const BETAS = ['managed-agents-2026-04-01'];
const MARKER = 'REMOTE-HAND-OK-9f31'; // must match models.rs REMOTE_HAND_MARKER

async function listEvents(client, sessionId) {
  const events = [];
  for await (const ev of client.beta.sessions.events.list(sessionId, { betas: BETAS })) events.push(ev);
  return events;
}

async function main() {
  await withServer('remote-hand', PORT, async (baseUrl) => {
    const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: baseUrl });

    const session = await client.beta.sessions.create({
      agent: 'assistant',
      environment_id: 'env_local',
      betas: BETAS,
    });

    await client.beta.sessions.events.send(session.id, {
      events: [{ type: 'user.message', content: [{ type: 'text', text: 'run the hand' }] }],
      betas: BETAS,
    });

    const events = await listEvents(client, session.id);

    // A server-side tool executed (the model issued a real tool call, not a
    // client/custom tool): the run shows a tool-use event.
    const toolUse = events.find((e) => e.type === 'agent.tool_use');
    assert.ok(toolUse, `expected a server-side tool_use, got: ${events.map((e) => e.type)}`);

    // The hand's `bash echo` stdout (the marker) round-tripped to the brain and
    // the model echoed it in its answer — the brain→hand→brain path end to end.
    const messages = events
      .filter((e) => e.type === 'agent.message')
      .map((e) => (e.content ?? []).map((c) => c.text ?? '').join(''));
    assert.ok(
      messages.some((m) => m.includes(MARKER)),
      `the hand's bash output must round-trip to the model: ${JSON.stringify(messages)}`,
    );

    const idle = [...events].reverse().find((e) => e.type === 'session.status_idle');
    assert.equal(idle.stop_reason.type, 'end_turn');

    console.log('E2E PASS: brain-hand — a served run executed bash on a remote hand and its output round-tripped (ADR-0044).');
  });
}

main().catch((err) => {
  console.error('E2E FAIL:', err);
  process.exit(1);
});
