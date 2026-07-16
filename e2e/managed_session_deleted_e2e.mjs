// session.deleted — DELETE emits a terminal stream event, verified through the
// official Anthropic TS SDK (offline, SDK-P oracle).
//
// Closes the conformance-matrix gap "session.deleted 缺 — DELETE 有,但不发终止
// 流事件": the SDK ships `session.deleted` in its event union, but the server
// never emitted it. Now DELETE commits a terminal `session.deleted` frame and
// pushes it to any open SSE stream *before* dropping the record. Delete is a real
// removal (not an archive tombstone), so the frame is live-broadcast only: a
// subsequent retrieve/events.list is a 404, by design.
//
// The observation window is a FRESH session (no turn): its stream backfill is
// non-terminal, so the SSE body tails the live broadcast instead of ending on a
// replayed idle. We open the stream, delete, and assert the SDK parses the
// terminal `session.deleted` type — then that the session is gone (404).
//
// Run: (from e2e/)  node managed_session_deleted_e2e.mjs

import assert from 'node:assert/strict';
import Anthropic from '@anthropic-ai/sdk';
import { withServer, pass } from './harness.mjs';

const BETAS = ['managed-agents-2026-04-01'];
const PORT = Number(process.env.E2E_PORT ?? 38431);
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

async function main() {
  try {
    await withServer('echo', PORT, async (baseUrl) => {
      const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: baseUrl });

      // A fresh session with no turn: its stream tails the live broadcast rather
      // than ending on a terminal backfill.
      const s = await client.beta.sessions.create({
        agent: 'assistant',
        environment_id: 'env_local',
        betas: BETAS,
      });

      // Start consuming the SSE stream in the background so the server-side
      // subscription is established before we delete. The stream ends on the
      // terminal `session.deleted` frame.
      const seen = [];
      const streaming = (async () => {
        const stream = await client.beta.sessions.events.stream(s.id, { betas: BETAS });
        for await (const ev of stream) {
          seen.push(ev.type);
          if (ev.type === 'session.deleted') break;
        }
      })();

      // Give the subscription a moment to attach, then delete.
      await sleep(400);
      await client.beta.sessions.delete(s.id, { betas: BETAS });

      // The stream must terminate on the delivered `session.deleted` frame.
      await Promise.race([
        streaming,
        sleep(5000).then(() => {
          throw new Error(`stream never delivered session.deleted (saw: ${seen.join(',') || '∅'})`);
        }),
      ]);
      assert.ok(
        seen.includes('session.deleted'),
        `the SDK parsed a terminal session.deleted frame (saw: ${seen.join(',') || '∅'})`,
      );
      pass('DELETE emits session.deleted on the open SSE stream (parsed by the official SDK)');

      // Delete is a real removal, not an archive tombstone: the session is gone.
      await assert.rejects(
        () => client.beta.sessions.retrieve(s.id, { betas: BETAS }),
        (err) => err.status === 404,
        'the deleted session retrieves as 404',
      );
      await assert.rejects(
        () => client.beta.sessions.events.list(s.id, { betas: BETAS })[Symbol.asyncIterator]().next(),
        (err) => err.status === 404,
        'events.list on the deleted session is a 404, not a replay',
      );
      pass('deleted session is gone — retrieve and events.list both 404 (no tombstone)');
    });

    console.log('E2E PASS: session.deleted stream event via the official TS SDK.');
    process.exitCode = 0;
  } catch (err) {
    console.error('E2E FAIL:', err);
    process.exitCode = 1;
  }
}

main();
