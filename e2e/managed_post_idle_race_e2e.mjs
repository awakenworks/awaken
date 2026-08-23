// §6.3 — the post-idle settle-before-cleanup gate, via the official Anthropic TS
// SDK against awaken-server (echo model).
//
// A live-push stream emits session.status_idle slightly before the session's
// queryable status settles, so a client that cleans up the instant it sees idle can
// race a still-"running" status. The safe protocol is the same regardless of how
// idle is observed: poll sessions.retrieve() until status !== 'running', THEN
// archive/delete. This test locks that ordering — retrieve settles off 'running',
// and archive then delete both succeed on the settled session. (The echo stream is
// replay-at-open with no live push, so the race window is zero here; the ordering
// gate is what we assert. The real live-push race is exercised in the real-model
// reconnect test.)
//
// Run: (from e2e/)  node managed_post_idle_race_e2e.mjs

import assert from 'node:assert/strict';
import Anthropic from '@anthropic-ai/sdk';
import {
  withRealServer,
  pass,
  waitForSessionEventReceipt,
  waitForValue,
} from './harness.mjs';

const BETAS = ['managed-agents-2026-04-01'];
const PORT = Number(process.env.E2E_PORT ?? 38403);
// The documented gate: after idle, poll until the queryable status is no longer
// 'running' before any cleanup call.
async function settle(client, sessionId) {
  return (await waitForValue(
    () => client.beta.sessions.retrieve(sessionId, { betas: BETAS }),
    (session) => session.status !== 'running',
    'queryable Session status to settle after committed idle',
    { timeoutMs: 2_000 },
  )).status;
}

async function sendAndObserveIdle(client, sessionId) {
  // C1=exact User receipt; C2=its idle event; C3=queryable status settles.
  // E1=C2 is post-C1; E2=C3 permits cleanup. K: event and Session projections
  // remain separate reads. Decision P1 C1&&!C2=>retry; P2 C1+C2=>settle C3.
  const receipt = await client.beta.sessions.events.send(sessionId, {
    events: [{ type: 'user.message', content: [{ type: 'text', text: 'work' }] }],
    betas: BETAS,
  });
  const receiptId = receipt.data[0]?.id;
  assert.equal(typeof receiptId, 'string', 'P1 exact post-idle User Event receipt');
  await waitForSessionEventReceipt(
    client,
    sessionId,
    receiptId,
    BETAS,
    ({ delta }) => delta.some((event) => event.type === 'session.status_idle'),
    'P1 exact Run idle to commit before Session-status settle',
  );
}

async function main() {
  try {
    await withRealServer('echo', PORT, async (baseUrl) => {
      const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: baseUrl });

      // --- archive after settling ---
      const a = await client.beta.sessions.create({ agent: 'assistant', environment_id: 'env_local', betas: BETAS });
      await sendAndObserveIdle(client, a.id);
      const settled = await settle(client, a.id);
      assert.notEqual(settled, 'running', 'status settled off running before cleanup');
      const archived = await client.beta.sessions.archive(a.id, { betas: BETAS });
      assert.ok(archived.archived_at, 'archive succeeds on the settled session');
      pass('turn sent -> retrieve settles off running -> archive succeeds (no write race)');

      // --- delete after settling ---
      const b = await client.beta.sessions.create({ agent: 'assistant', environment_id: 'env_local', betas: BETAS });
      await sendAndObserveIdle(client, b.id);
      await settle(client, b.id);
      await client.beta.sessions.delete(b.id, { betas: BETAS });
      await assert.rejects(
        () => client.beta.sessions.retrieve(b.id, { betas: BETAS }),
        (err) => err.status === 404,
      );
      pass('settle -> delete succeeds -> the session is gone (404)');
    });

    console.log('E2E PASS: post-idle settle-before-cleanup gate holds via TS SDK.');
    process.exitCode = 0;
  } catch (err) {
    console.error('E2E FAIL:', err);
    process.exitCode = 1;
  }
}

main();
