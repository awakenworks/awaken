// Context compaction e2e: once a session's transcript passes the (low)
// threshold, the compaction plugin folds the older turns into a summary via
// the `compactor` sub-agent and injects that summary request-only on later
// turns. The `compaction` mode's model reports the context it received, so the
// fold → summarize → inject loop is observable on the wire.

import assert from 'node:assert/strict';
import Anthropic from '@anthropic-ai/sdk';
import { withScenarioServer, pass } from './harness.mjs';

const BETAS = ['managed-agents-2026-04-01'];

async function reply(client, sessionId) {
  const events = [];
  for await (const ev of client.beta.sessions.events.list(sessionId, { betas: BETAS })) events.push(ev);
  return events
    .filter((e) => e.type === 'agent.message')
    .map((e) => e.content.map((b) => b.text ?? '').join(''));
}

async function turn(client, sessionId, text) {
  await client.beta.sessions.events.send(sessionId, {
    betas: BETAS,
    events: [{ type: 'user.message', content: [{ type: 'text', text }] }],
  });
  return reply(client, sessionId);
}

async function main() {
  await withScenarioServer('compaction', 'compaction', 38198, async (baseUrl) => {
    const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: baseUrl });
    const s = await client.beta.sessions.create({ agent: 'assistant', betas: BETAS });

    // Drive enough turns to cross the threshold (2) so the older slice folds.
    await turn(client, s.id, 'turn one');
    await turn(client, s.id, 'turn two');
    await turn(client, s.id, 'turn three');
    const last = await turn(client, s.id, 'turn four');

    // The compactor summary is injected request-only; the model surfaces the
    // context it saw, so the folded summary shows up in a later reply.
    assert.ok(
      last.some((m) => m.includes('SUMMARY: earlier turns folded')),
      `a later turn sees the folded summary in its injected context: ${JSON.stringify(last)}`,
    );
    pass('compaction: older turns fold into a summary injected on later turns');
  });
  console.log('E2E PASS: context compaction (fold + summarize + inject) across turns.');
}

main().catch((err) => {
  console.error('E2E FAIL:', err);
  process.exit(1);
});
