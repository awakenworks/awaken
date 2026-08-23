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
import {
  spawnServer,
  stopServer,
  waitForPort,
  waitForSessionEventReceipt,
  waitForValue,
  pass,
  startUpstream,
  realServerEnv,
} from './harness.mjs';

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
      let emptyReceipt;
      try {
        emptyReceipt = await client.beta.sessions.events.send(session.id, {
          events: [{ type: 'user.message', content: [] }],
          betas: BETAS,
        });
      } catch (err) {
        emptyStatus = err?.status ?? 500;
      }
      assert.ok(emptyStatus < 500, `empty content is not a server error (got ${emptyStatus})`);
      if (emptyReceipt) {
        await waitForSessionEventReceipt(
          client,
          session.id,
          emptyReceipt.data[0]?.id,
          BETAS,
          ({ delta }) => delta.some((event) => event.type === 'session.status_idle')
            || delta.some((event) => event.type === 'session.error'),
          'R3 accepted empty input reaches an explicit terminal public effect',
        );
      }
      const huge = 'x'.repeat(200_000);
      // R3: C1=huge input is accepted; C2=its exact receipt and reply commit.
      // E1=history contains the accepted Run. Constraint: the optional empty
      // input remains a synchronous accept/reject oracle. C1&&!C2=>observe;
      // C1+C2=>E1.
      const hugeReceipt = await client.beta.sessions.events.send(session.id, {
        events: [{ type: 'user.message', content: [{ type: 'text', text: huge }] }],
        betas: BETAS,
      });
      const { events: evs } = await waitForSessionEventReceipt(
        client,
        session.id,
        hugeReceipt.data[0]?.id,
        BETAS,
        ({ delta }) => delta.some((event) => event.type === 'agent.message'),
        'R3 huge message receipt reaches a committed reply',
      );
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
      // R4: C1=zero or more concurrent sends are admitted; C2=every admitted
      // exact receipt reaches a later idle. E1=the shared Session settles.
      // Constraint: clean per-send 4xx results stay allowed. Each C1+C2=>E1;
      // no admitted receipt is a test failure because it would prove no work.
      const acceptedIds = results.flatMap((result) => result?.data ?? [])
        .map((event) => event.id)
        .filter((id) => typeof id === 'string');
      assert.ok(acceptedIds.length > 0, 'at least one concurrent send is admitted');
      const observations = await Promise.all(acceptedIds.map((receiptId) => (
        waitForSessionEventReceipt(
          client,
          session.id,
          receiptId,
          BETAS,
          ({ delta }) => delta.some((event) => event.type === 'session.status_idle'),
          `R4 concurrent receipt ${receiptId} reaches idle`,
        )
      )));
      const idle = [...observations.at(-1).events]
        .reverse()
        .find((event) => event.type === 'session.status_idle');
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
      // R6: C1=first exact receipt settles; C2=a second receipt then settles.
      // E1=two committed replies share one Session. Constraint: C2 is admitted
      // only after C1 terminal, so a Busy race cannot masquerade as reuse.
      // C1&&!C2=>one reply; C1+C2=>E1.
      const first = await client.beta.sessions.events.send(session.id, {
        events: [{ type: 'user.message', content: [{ type: 'text', text: 'first' }] }],
        betas: BETAS,
      });
      await waitForSessionEventReceipt(
        client,
        session.id,
        first.data[0]?.id,
        BETAS,
        ({ delta }) => delta.some((event) => event.type === 'agent.message')
          && delta.some((event) => event.type === 'session.status_idle'),
        'R6 first receipt settles',
      );
      const second = await client.beta.sessions.events.send(session.id, {
        events: [{ type: 'user.message', content: [{ type: 'text', text: 'second' }] }],
        betas: BETAS,
      });
      const { events } = await waitForSessionEventReceipt(
        client,
        session.id,
        second.data[0]?.id,
        BETAS,
        ({ delta }) => delta.some((event) => event.type === 'agent.message')
          && delta.some((event) => event.type === 'session.status_idle'),
        'R6 second receipt settles',
      );
      const msgs = events.filter((event) => event.type === 'agent.message');
      assert.ok(msgs.length >= 2, 'a second turn runs on the same thread after the first');
      pass('thread operations work across successive runs');
    }

    // 7. A retryable upstream fault is recovered transparently inside the same
    // claimed attempt. `session.status_rescheduled` belongs only to durable
    // dispatch/claim replacement, so this provider retry must not fabricate it.
    {
      const retryUpstream = await startUpstream('echo', { failuresBeforeSuccess: 1, faultStatus: 503 });
      const retryServer = spawnServer('real', PORT + 1, { ...realServerEnv('echo', retryUpstream) });
      try {
        await waitForPort(PORT + 1);
        const retryClient = new Anthropic({ apiKey: 'e2e-dummy', baseURL: retryServer.baseUrl });
        const session = await retryClient.beta.sessions.create({ agent: 'assistant', environment_id: 'env_local', betas: BETAS });
        // R7: C1=exact input receipt; C2=the upstream records exactly one 503
        // plus one successful attempt; C3=reply and final idle commit. Effects:
        // E1=C2 proves a real provider retry; E2=C3 proves terminal recovery;
        // E3=no dispatch-owned reschedule marker is fabricated. Constraint:
        // both provider attempts retain one Run claim. Decision rules:
        // C1+C2&&!C3=>observe; C1+C2+C3=>E1+E2+E3.
        const receipt = await retryClient.beta.sessions.events.send(session.id, {
          events: [{ type: 'user.message', content: [{ type: 'text', text: 'retry-me' }] }],
          betas: BETAS,
        });
        const { events } = await waitForSessionEventReceipt(
          retryClient,
          session.id,
          receipt.data[0]?.id,
          BETAS,
          ({ delta }) => delta.some((event) => event.type === 'agent.message')
            && delta.some((event) => event.type === 'session.status_idle'),
          'R7 transparent provider retry reaches reply and final idle',
        );
        const types = events.map((event) => event.type);
        assert.equal(retryUpstream.attempts, 2, 'one retryable 503 causes exactly two provider attempts');
        assert.ok(!types.includes('session.status_rescheduled'), `provider retry fabricated dispatch reschedule: ${types}`);
        assert.ok(types.includes('agent.message'), `retry did not complete: ${types}`);
        assert.equal(types.at(-1), 'session.status_idle', `retry did not reach final idle: ${types}`);
        pass('same-attempt provider retry -> successful turn without dispatch reschedule');
      } finally {
        await stopServer(retryServer.server);
        retryUpstream.close();
      }
    }

    // 8. Interrupt an active turn, then steer the same idle session with a new
    // user message. The neutral runtime owns the cancellation/continuation
    // semantics; Managed only projects the resulting event sequence.
    {
      const slowUpstream = await startUpstream('echo', { delayMs: 900 });
      const steerServer = spawnServer('real', PORT + 2, { ...realServerEnv('echo', slowUpstream) });
      try {
        await waitForPort(PORT + 2);
        const steerClient = new Anthropic({ apiKey: 'e2e-dummy', baseURL: steerServer.baseUrl });
        const session = await steerClient.beta.sessions.create({ agent: 'assistant', environment_id: 'env_local', betas: BETAS });
        const active = steerClient.beta.sessions.events.send(session.id, {
          events: [{ type: 'user.message', content: [{ type: 'text', text: 'long-running original' }] }],
          betas: BETAS,
        });
        // R8: C1=the original Run reaches running and its exact User receipt is
        // retained; C2=an exact interrupt receipt reaches idle; C3=a replacement
        // receipt reaches its reply. Effects: E1=no timing guess and both C1/C2
        // receipts process; E2=same-Session replacement completes. Constraint:
        // committed running is the readiness authority, while exact receipts are
        // fenced only after the interrupt terminal edge. C1+C2+C3=>E1+E2.
        await waitForValue(
          () => listEvents(steerClient, session.id),
          (events) => events.some((event) => event.type === 'session.status_running'),
          'R8 original Run reaches committed running before interrupt',
          { timeoutMs: 10_000, pollMs: 10 },
        );
        const interruptReceipt = await steerClient.beta.sessions.events.send(session.id, {
          events: [{ type: 'user.interrupt' }],
          betas: BETAS,
        });
        await waitForSessionEventReceipt(
          steerClient,
          session.id,
          interruptReceipt.data[0]?.id,
          BETAS,
          ({ delta }) => delta.some((event) => event.type === 'session.status_idle'),
          'R8 interrupt receipt reaches idle',
        );
        const activeReceipt = (await active).data[0];
        assert.equal(activeReceipt?.type, 'user.message', 'R8 exact original User receipt');
        await waitForSessionEventReceipt(
          steerClient,
          session.id,
          activeReceipt.id,
          BETAS,
          () => true,
          'R8 original User receipt to process after interrupt idle',
        );
        const replacementReceipt = await steerClient.beta.sessions.events.send(session.id, {
          events: [{ type: 'user.message', content: [{ type: 'text', text: 'steered replacement' }] }],
          betas: BETAS,
        });
        const { delta } = await waitForSessionEventReceipt(
          steerClient,
          session.id,
          replacementReceipt.data[0]?.id,
          BETAS,
          ({ delta: current }) => current.some((event) => event.type === 'agent.message')
            && current.some((event) => event.type === 'session.status_idle'),
          'R8 replacement receipt reaches its terminal reply',
        );
        const texts = delta.filter((event) => event.type === 'agent.message')
          .flatMap((event) => event.content ?? []).map((part) => part.text ?? '').join(' ');
        assert.ok(texts.includes('steered replacement'), `steered answer missing: ${texts}`);
        pass('active interrupt followed by a replacement user.message completes on the same session');
      } finally {
        await stopServer(steerServer.server);
        slowUpstream.close();
      }
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
