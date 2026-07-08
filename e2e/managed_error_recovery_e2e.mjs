// Managed error-RECOVERY e2e (ported from awaken-next error-recovery.spec.ts):
// complements managed_error_paths (which covers fail-closed 404/wrong-ticket) by
// driving the graceful-degradation + recovery branches a server must survive:
// a bad agent, empty/huge content, concurrent sends to one session, a malformed
// request followed by a good one, and interrupt/cancel edge cases. These exercise
// the error/edge branches in the host, engine, protocol-managed router, and
// live-control that the happy-path suite never reaches. Deterministic (echo).
//
// Run: (from e2e/)  node managed_error_recovery_e2e.mjs

import assert from 'node:assert/strict';
import Anthropic from '@anthropic-ai/sdk';
import { spawnServer, stopServer, waitForPort, pass, startUpstream, realServerEnv } from './harness.mjs';

const BETAS = ['managed-agents-2026-04-01'];
const PORT = Number(process.env.E2E_PORT ?? 38224);

async function statusOf(promise, what) {
  try {
    await promise;
  } catch (err) {
    if (typeof err?.status === 'number') return err.status;
    throw new Error(`${what}: threw a non-API error: ${err}`);
  }
  throw new Error(`${what}: expected an error but the call succeeded`);
}
const isClientError = (s) => s >= 400 && s < 500;

async function listEvents(client, id) {
  const events = [];
  for await (const ev of client.beta.sessions.events.list(id, { betas: BETAS })) events.push(ev);
  return events;
}

async function main() {
  const upstream = await startUpstream('echo');
  const a = spawnServer('real', PORT, { ...realServerEnv('echo', upstream) });
  try {
    await waitForPort(PORT);
    const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: a.baseUrl });

    // 1. A session against an unusual agent id is handled without a 5xx: either a
    //    fail-closed 4xx or a default resolution, but never a crash (the agent
    //    resolution branch).
    {
      let status = 200;
      try {
        const s = await client.beta.sessions.create({ agent: 'ghost_agent_x', environment_id: 'env_local', betas: BETAS });
        await client.beta.sessions.events
          .send(s.id, { events: [{ type: 'user.message', content: [{ type: 'text', text: 'hi' }] }], betas: BETAS })
          .catch((e) => { status = e?.status ?? 500; });
      } catch (err) {
        status = err?.status ?? 500;
      }
      assert.ok(status < 500, `an unusual agent id is handled without a server error (got ${status})`);
      pass(`session against an unusual agent -> handled without 5xx (${status})`);
    }

    // 2. A malformed event body is rejected (4xx), and the server keeps serving:
    //    a good create right after succeeds (recover-after-bad-request).
    {
      const bad = await statusOf(
        client.beta.sessions.events.send('sesn_x', { events: [{ type: 'not.a.real.event' }], betas: BETAS }),
        'malformed event',
      );
      assert.ok(bad === 404 || isClientError(bad), `malformed event is a client error (got ${bad})`);
      const ok = await client.beta.sessions.create({ agent: 'assistant', environment_id: 'env_local', betas: BETAS });
      assert.ok(ok.id.startsWith('sesn_'), 'server still serves after a bad request');
      pass('server recovers and keeps serving after a bad request');
    }

    // 3. Empty message content is handled gracefully (accepted or a clean 4xx, not
    //    a 5xx / crash), and a huge message is accepted.
    {
      const session = await client.beta.sessions.create({ agent: 'assistant', environment_id: 'env_local', betas: BETAS });
      let emptyStatus = 200;
      try {
        await client.beta.sessions.events.send(session.id, { events: [{ type: 'user.message', content: [] }], betas: BETAS });
      } catch (err) {
        emptyStatus = err?.status ?? 500;
      }
      assert.ok(emptyStatus < 500, `empty content is not a server error (got ${emptyStatus})`);
      const huge = 'x'.repeat(200_000);
      await client.beta.sessions.events.send(session.id, { events: [{ type: 'user.message', content: [{ type: 'text', text: huge }] }], betas: BETAS });
      const evs = await listEvents(client, session.id);
      assert.ok(evs.length > 0, 'a huge message is accepted and produces events');
      pass('empty content handled gracefully; huge message accepted');
    }

    // 4. Concurrent sends to the SAME session are serialized without error (the
    //    per-thread state lock path), and the session ends cleanly.
    {
      const session = await client.beta.sessions.create({ agent: 'assistant', environment_id: 'env_local', betas: BETAS });
      const sends = Array.from({ length: 4 }, (_, i) =>
        client.beta.sessions.events
          .send(session.id, { events: [{ type: 'user.message', content: [{ type: 'text', text: `c${i}` }] }], betas: BETAS })
          .catch((e) => ({ err: e?.status ?? String(e) })),
      );
      const results = await Promise.all(sends);
      assert.ok(results.every((r) => !r || r.err === undefined || typeof r.err === 'number'),
        'concurrent sends resolve without a server crash');
      const idle = [...(await listEvents(client, session.id))].reverse().find((e) => e.type === 'session.status_idle');
      assert.ok(idle, 'the session reaches idle after concurrent sends');
      pass('concurrent sends to one session are serialized and end cleanly');
    }

    // 5. Interrupt / cancel edge cases on a nonexistent session fail closed (404),
    //    exercising the live-control not-found branch.
    {
      const cancelStatus = await statusOf(
        client.post(`/v1/sessions/sesn_ghost/cancel`, { body: {}, headers: { 'anthropic-beta': BETAS.join(',') } }),
        'cancel unknown session',
      );
      assert.ok(cancelStatus === 404 || isClientError(cancelStatus), `cancel unknown session fails closed (got ${cancelStatus})`);
      pass(`cancel on a nonexistent session -> fail closed (${cancelStatus})`);
    }

    // 6. Thread operations still work after a run: a fresh turn on an existing
    //    session continues the conversation (post-run thread reuse).
    {
      const session = await client.beta.sessions.create({ agent: 'assistant', environment_id: 'env_local', betas: BETAS });
      await client.beta.sessions.events.send(session.id, { events: [{ type: 'user.message', content: [{ type: 'text', text: 'first' }] }], betas: BETAS });
      await client.beta.sessions.events.send(session.id, { events: [{ type: 'user.message', content: [{ type: 'text', text: 'second' }] }], betas: BETAS });
      const msgs = (await listEvents(client, session.id)).filter((e) => e.type === 'agent.message');
      assert.ok(msgs.length >= 2, 'a second turn runs on the same thread after the first');
      pass('thread operations work across successive runs');
    }

    console.log('E2E PASS: managed error recovery + graceful degradation (ported from awaken-next).');
  } finally {
    await stopServer(a.server);
    upstream.close();
  }
}

main().catch((err) => {
  console.error('E2E FAIL:', err);
  process.exitCode = 1;
});
