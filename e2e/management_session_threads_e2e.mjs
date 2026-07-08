// The per-thread views of the Managed Agents sessions API, driven by the official
// Anthropic TS SDK: `client.beta.sessions.threads.archive` and
// `client.beta.sessions.threads.events.list / stream`. These three routes were
// wired but had no e2e — a fresh single-thread session exposes one primary thread
// whose per-thread event list/stream mirror the session's own events, and whose
// archive stamps `archived_at`. The model runs for real through the echo upstream.
//
// Run: (from e2e/)  node management_session_threads_e2e.mjs

import assert from 'node:assert/strict';
import Anthropic from '@anthropic-ai/sdk';
import { withRealServer, pass } from './harness.mjs';

const PORT = Number(process.env.E2E_PORT ?? 38231);
const BETAS = ['managed-agents-2026-04-01'];

async function drain(pagePromise) {
  const items = [];
  for await (const item of pagePromise) items.push(item);
  return items;
}

async function main() {
  await withRealServer('echo', PORT, async (baseUrl) => {
    const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: baseUrl });

    const session = await client.beta.sessions.create({
      agent: 'assistant',
      environment_id: 'env_local',
      betas: BETAS,
    });

    // One turn, so there are committed events to view per-thread.
    await client.beta.sessions.events.send(session.id, {
      events: [{ type: 'user.message', content: [{ type: 'text', text: 'hi there' }] }],
      betas: BETAS,
    });
    const sessionEvents = await drain(client.beta.sessions.events.list(session.id, { betas: BETAS }));
    assert.deepEqual(sessionEvents.map((e) => e.type), ['session.status_running', 'agent.message', 'session.status_idle']);

    // The session's single primary thread.
    const threads = await drain(client.beta.sessions.threads.list(session.id, { betas: BETAS }));
    assert.equal(threads.length, 1, 'a fresh session has one primary thread');
    const thread = threads[0];
    assert.equal(thread.archived_at, null, 'the primary thread starts un-archived');

    // -- threads.events.list mirrors the session events -----------------------
    const threadEvents = await drain(
      client.beta.sessions.threads.events.list(thread.id, { session_id: session.id, betas: BETAS }),
    );
    assert.deepEqual(
      threadEvents.map((e) => e.type),
      sessionEvents.map((e) => e.type),
      'per-thread event list mirrors the session events',
    );
    assert.equal(
      threadEvents.find((e) => e.type === 'agent.message').content[0].text,
      'Echo: hi there',
    );
    pass('beta.sessions.threads.events.list -> primary thread events');

    // -- threads.events.stream delivers the same events over SSE --------------
    const stream = await client.beta.sessions.threads.events.stream(thread.id, {
      session_id: session.id,
      betas: BETAS,
    });
    const streamed = [];
    for await (const ev of stream) streamed.push(ev.type);
    assert.ok(streamed.includes('agent.message'), `stream types: ${streamed}`);
    assert.ok(streamed.includes('session.status_idle'), `stream types: ${streamed}`);
    pass('beta.sessions.threads.events.stream -> SSE frames for the thread');

    // -- an unknown thread id is a 404 on every per-thread view ---------------
    await assert.rejects(
      () => client.beta.sessions.threads.retrieve('sthr_nope', { session_id: session.id, betas: BETAS }),
      (err) => err.status === 404,
    );
    await assert.rejects(
      () => drain(client.beta.sessions.threads.events.list('sthr_nope', { session_id: session.id, betas: BETAS })),
      (err) => err.status === 404,
    );
    pass('unknown thread id -> 404 on retrieve + events.list');

    // -- threads.archive stamps archived_at (and archives the session) --------
    const archivedThread = await client.beta.sessions.threads.archive(thread.id, {
      session_id: session.id,
      betas: BETAS,
    });
    assert.ok(archivedThread.archived_at, 'the archived thread carries archived_at');
    const gotSession = await client.beta.sessions.retrieve(session.id, { betas: BETAS });
    assert.ok(gotSession.archived_at, 'archiving the primary thread archives the session');
    pass('beta.sessions.threads.archive -> archived_at');

    await assert.rejects(
      () => client.beta.sessions.threads.archive('sthr_nope', { session_id: session.id, betas: BETAS }),
      (err) => err.status === 404,
    );
    pass('archive unknown thread -> 404');
  });

  console.log('E2E PASS: Managed Agents session-thread archive + per-thread event list/stream via TS SDK.');
}

main().catch((err) => {
  console.error('E2E FAIL:', err);
  process.exit(1);
});
