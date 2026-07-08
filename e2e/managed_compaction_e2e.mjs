// Context compaction e2e: once a session's transcript passes the (low)
// threshold, the compaction plugin folds the older turns into a summary via
// the `compactor` sub-agent and injects that summary request-only on later
// turns. The `compaction` mode's model reports the context it received, so the
// fold → summarize → inject loop is observable on the wire — AND the fold is
// projected as an `agent.thread_context_compacted` event (ADR-0047 D3),
// asserted below to precede the folded turn's message and to carry a
// `pre_compaction_tokens` estimate.

import assert from 'node:assert/strict';
import Anthropic from '@anthropic-ai/sdk';
import { withScenarioServer, pass } from './harness.mjs';

const BETAS = ['managed-agents-2026-04-01'];

async function allEvents(client, sessionId) {
  const events = [];
  for await (const ev of client.beta.sessions.events.list(sessionId, { betas: BETAS })) events.push(ev);
  return events;
}

async function reply(client, sessionId) {
  return (await allEvents(client, sessionId))
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

    // The fold is projected onto the event stream as `agent.thread_context_compacted`.
    const evs = await allEvents(client, s.id);
    const types = evs.map((e) => e.type);
    const at = types.indexOf('agent.thread_context_compacted');
    assert.ok(at >= 0, `the stream carries a compaction event: ${types.join(',')}`);

    // It carries a positive best-effort token estimate of the folded slice.
    const tokens = evs[at].pre_compaction_tokens;
    assert.ok(
      typeof tokens === 'number' && tokens > 0,
      `pre_compaction_tokens is a positive estimate: ${JSON.stringify(evs[at])}`,
    );

    // It runs at BeforeInference, so the marker precedes its turn's agent.message.
    const followingMessage = types.indexOf('agent.message', at + 1);
    assert.ok(
      followingMessage > at,
      `the compaction marker precedes a following agent.message: ${types.join(',')}`,
    );
    pass('compaction: agent.thread_context_compacted projected before the folded turn message');
  });
  console.log('E2E PASS: context compaction (fold + summarize + inject) across turns.');
}

main().catch((err) => {
  console.error('E2E FAIL:', err);
  process.exit(1);
});
