// The per-thread views of the Managed Agents sessions API, driven by the official
// Anthropic TS SDK: `client.beta.sessions.threads.archive` and
// `client.beta.sessions.threads.events.list / stream`. These three routes were
// wired but had no e2e — a fresh single-thread session exposes one primary thread
// whose per-thread event list/stream mirror the session's own events, and whose
// archive stamps `archived_at`. The model runs for real through the echo upstream.
//
// Run: (from e2e/)  node management_session_threads_e2e.mjs
//
// Cause graph:
//   accepted Session -> primary Thread -> committed Run -> list/stream views
//   primary archive -> Session terminal transition + Thread archived projection
//   unknown Thread -X-> event lookup/archive mutation
// Causes: primary/unknown Thread identity, committed/idle state, and requested
// list/stream/retrieve/archive operation. Effects: typed projection, ordered
// committed Events, one terminal archive, or fail-closed 404 without mutation.
//
// Decision table:
// | Thread | operation | prior state | result | authoritative effect |
// |---|---|---|---|---|
// | primary | list/retrieve | idle | typed Thread | same Session/agent snapshot |
// | primary | events list/stream | committed Run | ordered events | same committed content |
// | primary | archive | idle | archived Thread | Thread terminated before Session terminal |
// | unknown/retired sentinel | retrieve/list events/archive | absent | 404 | Session/events unchanged |
// Effects are the four table outcomes. Constraints/invariant: the primary
// Thread projection derives from the Session's committed Thread/Run facts and
// archive cannot invent a second lifecycle or mutate unknown ids.
// Decision rules are the table rows above, including the absent-id negative arm.

import assert from 'node:assert/strict';
import Anthropic from '@anthropic-ai/sdk';
import { withRealServer, pass, waitForSessionEventReceipt } from './harness.mjs';

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

    // One Run, so there are committed events to view per Thread.
    const receipt = await client.beta.sessions.events.send(session.id, {
      events: [{ type: 'user.message', content: [{ type: 'text', text: 'hi there' }] }],
      betas: BETAS,
    });
    // One admitted input is the cause; the durable lifecycle effects follow in
    // order and the primary-thread projection derives from this complete ledger.
    // C1=exact receipt; C2=usage and aggregate idle both commit. E1=complete
    // ordered Run with usage after primary Thread idle and before Session idle.
    // K=older history is ineligible. C1&&!C2=>observe; C1+C2=>E1.
    const { events: sessionEvents } = await waitForSessionEventReceipt(
      client,
      session.id,
      receipt.data[0]?.id,
      BETAS,
      ({ delta }) => delta.some((event) => event.type === 'session.usage')
        && delta.some((event) => event.type === 'session.status_idle'),
      'primary Thread Run commits through its usage snapshot',
    );
    assert.deepEqual(sessionEvents.map((e) => e.type), [
      'user.message',
      'session.status_running',
      'session.thread_status_running',
      'span.model_request_start',
      'span.model_request_end',
      'agent.message',
      'session.thread_status_idle',
      'session.usage',
      'session.status_idle',
    ]);

    // The session's single primary thread.
    const threads = await drain(client.beta.sessions.threads.list(session.id, { betas: BETAS }));
    assert.equal(threads.length, 1, 'a fresh session has one primary thread');
    const thread = threads[0];
    assert.equal(thread.archived_at, null, 'the primary thread starts un-archived');
    assert.equal(thread.type, 'session_thread');
    assert.equal(thread.session_id, session.id);
    assert.equal(thread.parent_thread_id, null);
    assert.match(thread.id, /^sthr_/u, 'the primary Thread id uses the official public prefix');
    assert.notEqual(thread.id, session.id, 'the internal Session root key never leaks as a Thread id');
    assert.ok(!thread.id.includes(':primary'), 'the retired primary sentinel never leaks');
    assert.equal(thread.status, 'idle');
    assert.equal(thread.agent.id, session.agent.id);
    assert.equal(thread.agent.multiagent, undefined, 'a Thread agent never repeats the Session roster');
    assert.equal(thread.stats, null);
    // T2 usage graph: C1=the sole primary Thread owns this Run; C2=the
    // `session.usage` ledger fact committed above. E1=list projects that exact
    // accounting onto the primary Thread; E2=retrieve returns the same snapshot.
    // K=no child Thread exists, so aggregate and primary usage are identical.
    // T2a C1&&!C2=>observe; T2b C1+C2=>E1+E2.
    const committedUsage = sessionEvents.find((event) => event.type === 'session.usage')?.usage;
    assert.ok(committedUsage, 'the complete Run owns a committed usage snapshot');
    assert.deepEqual(thread.usage, committedUsage, 'primary Thread list projects exact committed usage');
    const retrievedThread = await client.beta.sessions.threads.retrieve(thread.id, {
      session_id: session.id,
      betas: BETAS,
    });
    assert.equal(retrievedThread.id, thread.id, 'retrieve reverses the listed public primary Thread id');
    assert.equal(retrievedThread.parent_thread_id, null);
    assert.equal(retrievedThread.status, 'idle');
    assert.deepEqual(retrievedThread.usage, thread.usage, 'primary Thread retrieve preserves exact usage');
    const primaryStatuses = sessionEvents.filter((event) => event.type.startsWith('session.thread_status_'));
    assert.deepEqual(
      primaryStatuses.map((event) => event.type),
      ['session.thread_status_running', 'session.thread_status_idle'],
      'the primary Thread owns the complete Run bracket',
    );
    assert.ok(
      primaryStatuses.every((event) => event.session_thread_id === thread.id),
      'every primary status uses the listed public Thread id',
    );

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
    const streamed = [];
    const threadStreamTypes = sessionEvents
      .map((event) => event.type)
      .filter((type) => type !== 'session.usage');
    const abort = new AbortController();
    const timeout = setTimeout(() => abort.abort(), 10_000);
    try {
      const stream = await client.beta.sessions.threads.events.stream(
        thread.id,
        { session_id: session.id, betas: BETAS },
        { signal: abort.signal },
      );
      for await (const ev of stream) {
        streamed.push(ev.type);
        if (streamed.length === threadStreamTypes.length) break;
      }
    } finally {
      clearTimeout(timeout);
    }
    assert.deepEqual(
      streamed,
      threadStreamTypes,
      'thread SSE excludes the session-wide usage snapshot',
    );
    assert.ok(streamed.includes('agent.message'), `stream types: ${streamed}`);
    assert.ok(streamed.includes('session.status_idle'), `stream types: ${streamed}`);
    pass('beta.sessions.threads.events.stream -> SSE frames for the thread');

    // -- an unknown thread id is a 404 on every per-thread view ---------------
    const eventsBeforeUnknownCommands = await drain(
      client.beta.sessions.events.list(session.id, { betas: BETAS }),
    );
    for (const invalidThreadId of ['sthr_nope', `${session.id}:primary`]) {
      await assert.rejects(
        () => client.beta.sessions.threads.retrieve(invalidThreadId, { session_id: session.id, betas: BETAS }),
        (err) => err.status === 404,
      );
      await assert.rejects(
        () => drain(client.beta.sessions.threads.events.list(invalidThreadId, { session_id: session.id, betas: BETAS })),
        (err) => err.status === 404,
      );
    }
    assert.deepEqual(
      (await drain(client.beta.sessions.events.list(session.id, { betas: BETAS }))).map((event) => event.id),
      eventsBeforeUnknownCommands.map((event) => event.id),
      'unknown Thread reads commit no event side effect',
    );
    pass('unknown and retired thread ids -> 404 on retrieve + events.list');

    // -- threads.archive stamps archived_at (and archives the session) --------
    const archivedThread = await client.beta.sessions.threads.archive(thread.id, {
      session_id: session.id,
      betas: BETAS,
    });
    assert.equal(archivedThread.id, thread.id, 'archive decodes and reprojects the same public primary Thread id');
    assert.equal(archivedThread.status, 'terminated');
    assert.ok(archivedThread.archived_at, 'the archived thread carries archived_at');
    const gotSession = await client.beta.sessions.retrieve(session.id, { betas: BETAS });
    assert.ok(gotSession.archived_at, 'archiving the primary thread archives the session');
    assert.equal(gotSession.status, 'terminated', 'primary archive uses the Session terminal lifecycle');
    const archivedEvents = await drain(client.beta.sessions.events.list(session.id, { betas: BETAS }));
    const threadTerminated = archivedEvents.findIndex(
      (event) => event.type === 'session.thread_status_terminated' && event.session_thread_id === thread.id,
    );
    const sessionTerminated = archivedEvents.findIndex((event) => event.type === 'session.status_terminated');
    assert.ok(
      threadTerminated >= 0 && threadTerminated < sessionTerminated,
      'primary Thread termination precedes the authoritative Session terminal event',
    );
    pass('beta.sessions.threads.archive -> archived_at');

    const eventsBeforeUnknownArchive = await drain(
      client.beta.sessions.events.list(session.id, { betas: BETAS }),
    );
    for (const invalidThreadId of ['sthr_nope', `${session.id}:primary`]) {
      await assert.rejects(
        () => client.beta.sessions.threads.archive(invalidThreadId, { session_id: session.id, betas: BETAS }),
        (err) => err.status === 404,
      );
    }
    assert.deepEqual(
      (await drain(client.beta.sessions.events.list(session.id, { betas: BETAS }))).map((event) => event.id),
      eventsBeforeUnknownArchive.map((event) => event.id),
      'an unknown Thread command commits no event side effect',
    );
    pass('archive unknown and retired thread ids -> 404');
  });

  console.log('E2E PASS: Managed Agents session-thread archive + per-thread event list/stream via TS SDK.');
}

main().catch((err) => {
  console.error('E2E FAIL:', err);
  process.exit(1);
});
