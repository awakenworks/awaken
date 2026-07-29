// Real-model `agent.thinking` preview/reconnect conformance (events/reference
// "Agent events"): a thinking-capable provider's extended thinking must first
// surface as one start-only preview, then as the contentless durable
// `agent.thinking` marker (`{id, processed_at, type}`). A connection dropped
// after that marker must recover the answer from a reopened stream/history without
// replaying the stream-only thinking preview.
//
// This validates the reasoning path end to end: genai captures the provider's
// reasoning -> a folded `Thinking` block -> `Fact::AssistantThinking` -> the Managed
// wire's `agent.thinking`. A thinking-capable provider emits reasoning for the
// step-by-step prompt below. The reasoning text itself is intentionally NOT on the
// wire (the marker carries none); the answer still lands in `agent.message`.
//
// Gated: skips without a real key. Run: (from e2e/, with KIMI env)
//   ANTHROPIC_API_KEY=sk-kimi-... ANTHROPIC_BASE_URL=https://api.kimi.com/coding/v1/ \
//   ANTHROPIC_MODEL=kimi-for-coding node managed_real_thinking_e2e.mjs

import assert from 'node:assert/strict';
import Anthropic from '@anthropic-ai/sdk';
import { withServer, pass } from './harness.mjs';

const PORT = Number(process.env.E2E_PORT ?? 38254);
const BETAS = ['managed-agents-2026-04-01'];
const STREAM_PARAMS = { betas: BETAS, event_deltas: ['agent.thinking'] };

async function listAll(client, sessionId) {
  const events = [];
  for await (const event of client.beta.sessions.events.list(sessionId, { betas: BETAS })) {
    events.push(event);
  }
  return events;
}

function assertContentlessThinking(mark, label) {
  assert.equal(mark.type, 'agent.thinking', `${label}: event type`);
  assert.ok(mark.id, `${label}: marker carries an id`);
  assert.ok(mark.processed_at, `${label}: durable marker carries processed_at`);
  assert.equal(mark.content, undefined, `${label}: marker carries no content field`);
  assert.equal(mark.thinking, undefined, `${label}: marker carries no thinking text`);
}

async function main() {
  if (!process.env.ANTHROPIC_API_KEY && !process.env.KIMI_API_KEY) {
    console.log('SKIP managed_real_thinking_e2e: no ANTHROPIC_API_KEY / KIMI_API_KEY set.');
    return;
  }
  try {
    await withServer('real', PORT, async (baseUrl) => {
      const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: baseUrl });
      const session = await client.beta.sessions.create({ agent: 'assistant', environment_id: 'env_local', betas: BETAS });

      // Cause-effect graph / decision table E3+E6:
      // C1=thinking preview opt-in; C2=provider emits reasoning; C3=connection
      // remains through the durable thinking marker; C4=connection drops before
      // turn completion; C5=client reopens the same opted-in stream and lists
      // history. E1=exactly one start with no thinking delta/content; E2=durable
      // marker reuses its id; E3=reconnect does not replay event_start/event_delta;
      // E4=history contains one marker and the complete answer reaches idle.
      // Constraint: preview frames are live-only and never enter events.list().
      //
      // | Rule | opt-in | reasoning | drop/reopen | Effects |
      // | T1   | yes    | yes       | yes         | E1,E2,E3,E4 |
      const firstStream = await client.beta.sessions.events.stream(
        session.id,
        STREAM_PARAMS,
        { signal: AbortSignal.timeout(180_000) },
      );
      const send = client.beta.sessions.events.send(session.id, {
        events: [{ type: 'user.message', content: [{ type: 'text', text: 'Think step by step, then answer: a bat and a ball cost $1.10 total, and the bat costs $1.00 more than the ball. How many cents is the ball? Reply with just the number.' }] }],
        betas: BETAS,
      }, { signal: AbortSignal.timeout(180_000) });

      const beforeDrop = [];
      for await (const event of firstStream) {
        beforeDrop.push(event);
        if (event.type === 'agent.thinking') break;
        assert.notEqual(
          event.type,
          'session.status_terminated',
          `turn terminated before thinking committed: ${JSON.stringify(event)}`,
        );
      }

      const starts = beforeDrop.filter(
        (event) => event.type === 'event_start' && event.event?.type === 'agent.thinking',
      );
      assert.equal(starts.length, 1, `T1/E1 expected one thinking start: ${JSON.stringify(beforeDrop)}`);
      const previewId = starts[0].event.id;
      assert.ok(previewId, 'T1/E1 thinking preview carries the upcoming durable id');
      assert.ok(
        beforeDrop.every(
          (event) => !(event.type === 'event_delta' && event.event_id === previewId),
        ),
        'T1/E1 thinking preview is start-only',
      );
      const firstMarker = beforeDrop.find((event) => event.type === 'agent.thinking');
      assert.ok(firstMarker, `T1/E2 expected the durable marker: ${JSON.stringify(beforeDrop)}`);
      assertContentlessThinking(firstMarker, 'T1/E2');
      assert.equal(firstMarker.id, previewId, 'T1/E2 durable marker reuses the preview id');
      pass('thinking preview is start-only and reconciles to one contentless durable marker');

      // Breaking the iterator closes the first SSE connection. Reopen immediately
      // while the original send may still be finishing, then drain through idle.
      const reopened = await client.beta.sessions.events.stream(
        session.id,
        STREAM_PARAMS,
        { signal: AbortSignal.timeout(180_000) },
      );
      const afterReconnect = [];
      for await (const event of reopened) afterReconnect.push(event);
      await send;

      assert.ok(
        afterReconnect.every(
          (event) => event.type !== 'event_start' && event.type !== 'event_delta',
        ),
        `T1/E3 reconnect must not replay previews: ${JSON.stringify(afterReconnect)}`,
      );
      assert.ok(
        afterReconnect.some((event) => event.type === 'session.status_idle'),
        `T1/E4 reopened stream reaches idle: ${JSON.stringify(afterReconnect)}`,
      );
      pass('reopened stream recovers the turn without replaying thinking preview frames');

      const events = await listAll(client, session.id);
      const thinking = events.filter((event) => event.type === 'agent.thinking');
      assert.equal(thinking.length, 1, `T1/E4 history contains one marker: ${JSON.stringify(events)}`);
      assertContentlessThinking(thinking[0], 'T1/E4');
      assert.equal(thinking[0].id, previewId, 'T1/E4 history preserves the reconciled id');
      assert.ok(
        events.every((event) => event.type !== 'event_start' && event.type !== 'event_delta'),
        'T1/E3 preview frames never enter authoritative history',
      );

      // Thinking precedes the answer, and the answer still lands in agent.message.
      const idxThink = events.findIndex((e) => e.type === 'agent.thinking');
      const idxMsg = events.findIndex((e) => e.type === 'agent.message');
      assert.ok(idxMsg === -1 || idxThink < idxMsg, 'agent.thinking is emitted before agent.message');
      const finalMsg = events.filter((e) => e.type === 'agent.message').at(-1);
      assert.ok(finalMsg, 'the answer still lands in agent.message');
      assert.match(JSON.stringify(finalMsg.content), /5/, `expected the correct answer (5 cents): ${JSON.stringify(finalMsg.content)}`);
      pass('answer lands in agent.message (5 cents); thinking precedes it');

      console.log('E2E PASS: real-model thinking preview is start-only, durable, and not replayed after reconnect.');
    });
  } catch (err) {
    console.error('E2E FAIL:', err);
    process.exit(1);
  }
}

main();
