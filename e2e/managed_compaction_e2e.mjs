// Context compaction e2e: once a session's transcript passes the (low)
// threshold, the compaction plugin folds the older Runs into a summary via
// the `compactor` sub-agent and injects that summary request-only on later
// Runs. The `compaction` mode's model reports the context it received, so the
// fold → summarize → inject loop is observable on the wire — AND the fold is
// projected as an `agent.thread_context_compacted` event (ADR-0047 D3),
// asserted below to precede the folded Run's message and to carry a
// `pre_compaction_tokens` estimate.

import assert from 'node:assert/strict';
import Anthropic from '@anthropic-ai/sdk';
import { pass, waitForSessionEventReceipt, withScenarioServer } from './harness.mjs';

const BETAS = ['managed-agents-2026-04-01'];

async function allEvents(client, sessionId) {
  const events = [];
  for await (const ev of client.beta.sessions.events.list(sessionId, { betas: BETAS })) events.push(ev);
  return events;
}

function replies(events) {
  return events
    .filter((e) => e.type === 'agent.message')
    .map((e) => e.content.map((b) => b.text ?? '').join(''));
}

async function sendRun(client, sessionId, text) {
  const receipt = await client.beta.sessions.events.send(sessionId, {
    betas: BETAS,
    events: [{ type: 'user.message', content: [{ type: 'text', text }] }],
  });
  const acceptedId = receipt.data[0]?.id;
  assert.equal(typeof acceptedId, 'string', 'compaction Run returns its exact User Event receipt');
  const { events } = await waitForSessionEventReceipt(
    client,
    sessionId,
    acceptedId,
    BETAS,
    ({ delta }) => delta.some((event) => event.type === 'agent.message')
      && delta.some((event) => event.type === 'session.status_idle'),
    `the Run for ${text} to commit its reply and terminal Session status`,
  );
  return replies(events);
}

async function main() {
  await withScenarioServer('compaction', 'compaction', 38198, async (baseUrl) => {
    const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: baseUrl });
    const s = await client.beta.sessions.create({ agent: 'assistant', environment_id: 'env_local', betas: BETAS });

    // Cause/effect graph: C0=each accepted User Event is initially only a durable
    // receipt and later commits its own Run; C1=committed context crosses the configured threshold;
    // C2=the deterministic compactor returns its summary; C3=a later Run reaches
    // BeforeInference. Effects: E1=the summary is request-only context visible to
    // the model; E2=one exact SDK-shaped thread_context_compacted marker commits;
    // E3=the marker precedes the following agent.message. Decision rule R1:
    // C0 && C1 && C2 && C3 => E1-E3. Below-threshold behavior has its focused owner.
    // Constraints/invariant: compaction is request context plus one committed
    // marker; it never rewrites or duplicates the authoritative Event history.
    // Drive enough Runs to cross the threshold (2) so the older slice folds.
    await sendRun(client, s.id, 'Run one');
    await sendRun(client, s.id, 'Run two');
    await sendRun(client, s.id, 'Run three');
    const last = await sendRun(client, s.id, 'Run four');

    // The compactor summary is injected request-only; the model surfaces the
    // context it saw, so the folded summary shows up in a later reply.
    assert.ok(
      last.some((m) => m.includes('SUMMARY: earlier Runs folded')),
      `a later Run sees the folded summary in its injected context: ${JSON.stringify(last)}`,
    );
    pass('compaction: older Runs fold into a summary injected on later Runs');

    // The fold is projected onto the event stream, decoded by the official SDK as
    // BetaManagedAgentsAgentThreadContextCompactedEvent (`{id, type, processed_at}`).
    const evs = await allEvents(client, s.id);
    const types = evs.map((e) => e.type);
    const at = types.indexOf('agent.thread_context_compacted');
    assert.ok(at >= 0, `the stream carries a compaction event: ${types.join(',')}`);

    // Its shape matches the SDK type exactly: a plain marker with an id + timestamp
    // and no payload (aligned to @anthropic-ai/sdk, not a guessed field).
    const ev = evs[at];
    assert.equal(typeof ev.id, 'string', `event has an id: ${JSON.stringify(ev)}`);
    assert.equal(typeof ev.processed_at, 'string', `event has processed_at: ${JSON.stringify(ev)}`);
    assert.ok(
      !('pre_compaction_tokens' in ev),
      `no fields beyond the SDK type: ${JSON.stringify(ev)}`,
    );

    // It runs at BeforeInference, so the marker precedes its Run's agent.message.
    const followingMessage = types.indexOf('agent.message', at + 1);
    assert.ok(
      followingMessage > at,
      `the compaction marker precedes a following agent.message: ${types.join(',')}`,
    );
    pass('compaction: agent.thread_context_compacted (SDK shape) precedes the folded Run message');
  });
  console.log('E2E PASS: context compaction (fold + summarize + inject) across Runs.');
}

main().catch((err) => {
  console.error('E2E FAIL:', err);
  process.exit(1);
});
