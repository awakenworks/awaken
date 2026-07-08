// Reconnect / stream-consolidation conformance for Managed Agents, driven by the
// official Anthropic TypeScript SDK against awaken-server (echo model).
//
// A real SSE client has no replay: on a dropped stream it must reopen AND fetch
// events.list() to fill the gap, deduping by event id (the pattern in the SDK's
// own managed-agents examples). This test locks the invariants consolidation
// depends on — event ids are stable + unique, events.list() is the authoritative,
// monotonically-growing history, and the live stream never carries an id absent
// from the list. The mid-turn-gap variant that needs a slow model to actually
// drop a turn in flight lives in managed_reconnect_real_e2e.mjs.
//
// Run: (from e2e/)  node managed_reconnect_e2e.mjs

import assert from 'node:assert/strict';
import Anthropic from '@anthropic-ai/sdk';
import { withRealServer, pass } from './harness.mjs';

const BETAS = ['managed-agents-2026-04-01'];
const PORT = Number(process.env.E2E_PORT ?? 38140);

async function listAll(client, sessionId) {
  const events = [];
  for await (const ev of client.beta.sessions.events.list(sessionId, { betas: BETAS })) events.push(ev);
  return events;
}

async function sendMessage(client, sessionId, text) {
  await client.beta.sessions.events.send(sessionId, {
    events: [{ type: 'user.message', content: [{ type: 'text', text }] }],
    betas: BETAS,
  });
}

function assertUniqueIds(events, label) {
  const seen = new Set();
  for (const ev of events) {
    assert.ok(ev.id, `${label}: every event carries an id (got ${JSON.stringify(ev.type)})`);
    assert.ok(!seen.has(ev.id), `${label}: event id ${ev.id} is not duplicated`);
    seen.add(ev.id);
  }
  return seen;
}

async function main() {
  try {
    await withRealServer('echo', PORT, async (baseUrl) => {
      const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: baseUrl });

      const session = await client.beta.sessions.create({
        agent: 'assistant',
        environment_id: 'env_local',
        betas: BETAS,
      });
      assert.ok(session.id.startsWith('sesn_'));
      pass(`session created: ${session.id}`);

      // --- A. ids are stable + unique; list() is the authoritative history ---
      await sendMessage(client, session.id, 'one');
      await sendMessage(client, session.id, 'two');
      const afterTwo = await listAll(client, session.id);
      const idsAfterTwo = assertUniqueIds(afterTwo, 'after two turns');
      assert.ok(
        afterTwo.some((e) => e.type === 'agent.message' && e.content?.[0]?.text === 'Echo: one'),
        'list() contains the first echo',
      );
      assert.ok(
        afterTwo.some((e) => e.type === 'agent.message' && e.content?.[0]?.text === 'Echo: two'),
        'list() contains the second echo',
      );
      pass('event ids are unique; list() holds the full history');

      // --- B. the SSE stream replays the persisted history (no live push in echo
      //        mode); draining it fully yields events whose ids all exist in
      //        list(). This is the "reopen the stream" half of a reconnect. ---
      await sendMessage(client, session.id, 'three');
      const stream = await client.beta.sessions.events.stream(session.id, { betas: BETAS });
      const streamed = [];
      for await (const ev of stream) streamed.push(ev);
      assert.ok(streamed.length > 0, 'the reopened stream delivered events');
      assert.ok(streamed.some((e) => e.type === 'agent.message'), 'the stream carries agent.message events');
      assert.ok(streamed.some((e) => e.type === 'session.status_idle'), 'the stream carries status_idle');
      assertUniqueIds(streamed.filter((e) => e.id), 'streamed');
      pass('the reopened stream replays the persisted turns');

      // --- C. consolidation: list() is monotonic and a superset of the stream;
      //        deduping (list ∪ stream) by id yields no phantom ids. ---
      const full = await listAll(client, session.id);
      const fullIds = assertUniqueIds(full, 'full history');
      assert.ok(
        full.some((e) => e.type === 'agent.message' && e.content?.[0]?.text === 'Echo: three'),
        'list() picked up the third turn',
      );
      for (const id of idsAfterTwo) {
        assert.ok(fullIds.has(id), `history is monotonic: id ${id} survives into the later list`);
      }
      for (const ev of streamed) {
        if (!ev.id) continue;
        assert.ok(fullIds.has(ev.id), `no phantom: streamed id ${ev.id} exists in list()`);
      }
      // The reconnect merge a client performs: history first, then the live tail,
      // skipping already-seen ids. It must converge to exactly list()'s id set.
      const merged = new Set();
      for (const ev of full) merged.add(ev.id);
      for (const ev of streamed) if (ev.id) merged.add(ev.id);
      assert.equal(merged.size, fullIds.size, 'consolidation adds no ids beyond the authoritative history');
      pass('reconnect consolidation (list ∪ stream, dedupe by id) is lossless and phantom-free');
    });

    console.log('E2E PASS: Managed Agents reconnect/stream-consolidation invariants via TS SDK.');
    process.exitCode = 0;
  } catch (err) {
    console.error('E2E FAIL:', err);
    process.exitCode = 1;
  }
}

main();
