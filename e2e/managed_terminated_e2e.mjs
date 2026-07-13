// §3 — the terminal transition of the session state machine, via the official
// Anthropic TS SDK against awaken-server (echo model).
//
// This server models archive as termination: POST /v1/sessions/:id/archive stamps
// archived_at, moves status to "terminated", and commits a session.status_terminated
// event. This test locks that terminal edge: the event appears on both events.list
// and events.stream (as the last event), retrieve reports terminated, the archive is
// idempotent (no duplicate terminated event), and the session is read-only (writes
// 409).
//
// Run: (from e2e/)  node managed_terminated_e2e.mjs

import assert from 'node:assert/strict';
import Anthropic from '@anthropic-ai/sdk';
import { withRealServer, pass } from './harness.mjs';

const BETAS = ['managed-agents-2026-04-01'];
const PORT = Number(process.env.E2E_PORT ?? 38407);

async function listAll(client, sid) {
  const evs = [];
  for await (const e of client.beta.sessions.events.list(sid, { betas: BETAS })) evs.push(e);
  return evs;
}

async function main() {
  try {
    await withRealServer('echo', PORT, async (baseUrl) => {
      const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: baseUrl });
      const session = await client.beta.sessions.create({ agent: 'assistant', environment_id: 'env_local', betas: BETAS });

      // A normal turn first, so the log has running<->idle history to terminate after.
      await client.beta.sessions.events.send(session.id, {
        events: [{ type: 'user.message', content: [{ type: 'text', text: 'hello' }] }],
        betas: BETAS,
      });
      const beforeIds = new Set((await listAll(client, session.id)).map((e) => e.id));

      // Archive == terminate.
      const archived = await client.beta.sessions.archive(session.id, { betas: BETAS });
      assert.ok(archived.archived_at, 'archive stamps archived_at');
      assert.equal(archived.status, 'terminated', 'archive returns status terminated');
      pass('archive -> archived_at + status terminated');

      // The terminated event is committed and is the last event on list().
      const events = await listAll(client, session.id);
      const terminated = events.filter((e) => e.type === 'session.status_terminated');
      assert.equal(terminated.length, 1, 'exactly one session.status_terminated event');
      assert.equal(events[events.length - 1].type, 'session.status_terminated', 'terminated is the last event');
      assert.ok(!beforeIds.has(terminated[0].id), 'the terminated event is newly minted');
      assert.ok(terminated[0].processed_at, 'the terminated event carries a processed_at');
      pass('events.list carries a single session.status_terminated as the final event');

      // The same event replays on the stream.
      const streamed = [];
      const stream = await client.beta.sessions.events.stream(session.id, { betas: BETAS });
      for await (const ev of stream) streamed.push(ev);
      assert.ok(
        streamed.some((e) => e.type === 'session.status_terminated'),
        `events.stream replays the terminated event (types: ${streamed.map((e) => e.type)})`,
      );
      pass('events.stream replays session.status_terminated');

      // Retrieve reflects the terminal state.
      const got = await client.beta.sessions.retrieve(session.id, { betas: BETAS });
      assert.equal(got.status, 'terminated');
      assert.ok(got.archived_at);

      // The session is read-only: a write is refused with 409.
      await assert.rejects(
        () =>
          client.beta.sessions.events.send(session.id, {
            events: [{ type: 'user.message', content: [{ type: 'text', text: 'nope' }] }],
            betas: BETAS,
          }),
        (err) => {
          assert.equal(err.status, 409, `write after terminate -> 409 (got ${err.status})`);
          assert.equal(err.error?.error?.type, 'invalid_request_error');
          return true;
        },
      );
      pass('terminated session is read-only: events.send -> 409');

      // Archive is idempotent: re-archiving mints no second terminated event.
      await client.beta.sessions.archive(session.id, { betas: BETAS });
      const after = await listAll(client, session.id);
      assert.equal(
        after.filter((e) => e.type === 'session.status_terminated').length,
        1,
        're-archive does not duplicate the terminated event',
      );
      pass('archive is idempotent: no duplicate terminated event');
    });

    console.log('E2E PASS: session terminal transition (archive -> terminated event, read-only) via TS SDK.');
    process.exitCode = 0;
  } catch (err) {
    console.error('E2E FAIL:', err);
    process.exitCode = 1;
  }
}

main();
