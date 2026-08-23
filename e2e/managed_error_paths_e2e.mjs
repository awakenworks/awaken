// Managed error-path e2e (ported from awaken-next exception cases): drives the
// managed sessions API through the official @anthropic-ai/sdk and asserts the
// fail-closed / not-found / wrong-ticket rejections a Claude-Managed-Agents server
// must enforce. All deterministic (probe mode). Ported scenarios:
//   - events to an unknown session            -> 404 not_found
//   - retrieve an unknown session             -> 404 not_found
//   - tool_confirmation with a WRONG tool_use_id on an awaiting run -> fail closed (4xx)
//   - unrelated user.message while a tool ticket is pending -> atomic 400; ticket retained
//   - tool_confirmation when nothing is awaiting -> fail closed (4xx)
//   - custom_tool_result with no matching ticket -> fail closed (4xx)
//
// Run: (from e2e/)  node managed_error_paths_e2e.mjs

import assert from 'node:assert/strict';
import Anthropic from '@anthropic-ai/sdk';
import {
  spawnServer,
  stopServer,
  waitForPort,
  waitForSessionEventReceipt,
  pass,
  startUpstream,
  realServerEnv,
} from './harness.mjs';

const BETAS = ['managed-agents-2026-04-01'];
const PORT = Number(process.env.E2E_PORT ?? 38222);

// Capture the SDK's thrown APIError and return its HTTP status (or rethrow if it
// unexpectedly succeeded). The SDK throws Anthropic.APIError with `.status`.
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

async function main() {
  const upstream = await startUpstream('probe');
  const a = spawnServer('real', PORT, { ...realServerEnv('probe', upstream) });
  try {
    await waitForPort(PORT);
    const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: a.baseUrl });

    // 1. Events to an unknown session -> 404 (fail closed; the store is authoritative).
    {
      const status = await statusOf(
        client.beta.sessions.events.send('sesn_does_not_exist', {
          events: [{ type: 'user.message', content: [{ type: 'text', text: 'hi' }] }],
          betas: BETAS,
        }),
        'send to unknown session',
      );
      assert.equal(status, 404, 'events to an unknown session are 404');
      pass('events to an unknown session -> 404 not_found');
    }

    // 2. Retrieve an unknown session -> 404.
    {
      const status = await statusOf(
        client.beta.sessions.retrieve('sesn_missing', { betas: BETAS }),
        'retrieve unknown session',
      );
      assert.equal(status, 404, 'retrieving an unknown session is 404');
      pass('retrieve an unknown session -> 404 not_found');
    }

    // 3. tool_confirmation with a WRONG tool_use_id on a genuinely awaiting run: the
    //    run awaiting on tool X; confirming a different id must fail closed, not
    //    resolve the real pending tool.
    {
      const session = await client.beta.sessions.create({
        agent: 'assistant',
        environment_id: 'env_local',
        betas: BETAS,
      });
      // Awaiting-boundary rule A1: C1=the exact User receipt commits and C2=its
      // tool_use plus requires_action idle follow it; E1=use that tool id for the
      // wrong-id rejection oracle. Constraint: older tool history is ineligible.
      // Decision: C1&&!C2=>observe again; C1+C2=>E1; wrong id=>synchronous 4xx.
      const receipt = await client.beta.sessions.events.send(session.id, {
        events: [{ type: 'user.message', content: [{ type: 'text', text: 'AWAIT-ME' }] }],
        betas: BETAS,
      });
      const { events: pendingEvents, delta } = await waitForSessionEventReceipt(
        client,
        session.id,
        receipt.data[0]?.id,
        BETAS,
        ({ delta: current }) => current.some((event) => event.type === 'agent.tool_use')
          && current.some((event) => event.type === 'session.status_idle'
            && event.stop_reason?.type === 'requires_action'),
        'A1 exact User receipt reaches requires_action',
      );
      const awaiting = delta.find((event) => event.type === 'agent.tool_use');
      assert.ok(awaiting, 'the run awaiting on a tool_use');
      const status = await statusOf(
        client.beta.sessions.events.send(session.id, {
          events: [
            { type: 'user.tool_confirmation', tool_use_id: 'toolu_bogus_id', result: 'allow' },
          ],
          betas: BETAS,
        }),
        'confirm with wrong tool_use_id',
      );
      assert.ok(isClientError(status), `wrong tool_use_id fails closed (got ${status})`);
      pass(`tool_confirmation with a wrong tool_use_id -> fail closed (${status})`);

      // Pending-ticket admission rule P1-P2. Causes: C1=the exact committed
      // tool-use Event remains unresolved after the wrong-id rejection;
      // C2=an unrelated User message does not resolve C1; C3=a later exact
      // allow names C1. Effects: E1=C1+C2 returns 400 without a receipt;
      // E2=history and Provider requests remain byte-for-byte/count stable;
      // E3=C1+C3 consumes the original ticket exactly once and the same Run
      // reaches its tool result, reply, and end_turn. Constraint/authority:
      // committed Event ids plus the ResumeTicket own pending custody; no cache
      // or timing observation may replace them. Decision table: P1(C1+C2)
      // ->E1+E2; P2(P1+C3)->E3.
      const eventIdsBeforeUnrelated = pendingEvents.map((event) => event.id);
      const providerRequestsBeforeUnrelated = upstream.requests.length;
      await assert.rejects(
        client.beta.sessions.events.send(session.id, {
          events: [{
            type: 'user.message',
            content: [{ type: 'text', text: 'UNRELATED-WHILE-PENDING' }],
          }],
          betas: BETAS,
        }),
        (error) => {
          assert.equal(error?.status, 400, 'P1/E1 unrelated User is rejected');
          assert.equal(error?.error?.type, 'error', 'P1/E1 uses the Anthropic error envelope');
          assert.equal(
            error?.error?.error?.type,
            'invalid_request_error',
            'P1/E1 is an admission error',
          );
          assert.equal(
            error?.error?.error?.message,
            'pending tool events must be resolved before user.message',
            'P1/E1 selects the unresolved-ticket branch',
          );
          return true;
        },
      );
      const eventIdsAfterUnrelated = [];
      for await (const event of client.beta.sessions.events.list(session.id, { betas: BETAS })) {
        eventIdsAfterUnrelated.push(event.id);
      }
      assert.deepEqual(
        eventIdsAfterUnrelated,
        eventIdsBeforeUnrelated,
        'P1/E2 rejected User appends no receipt or lifecycle Event',
      );
      assert.equal(
        upstream.requests.length,
        providerRequestsBeforeUnrelated,
        'P1/E2 rejected User performs no Provider request',
      );

      const allowReceipt = await client.beta.sessions.events.send(session.id, {
        events: [{
          type: 'user.tool_confirmation',
          tool_use_id: awaiting.id,
          result: 'allow',
        }],
        betas: BETAS,
      });
      const allowReceiptId = allowReceipt.data[0]?.id;
      assert.equal(typeof allowReceiptId, 'string', 'P2/C3 exact allow receipt');
      const { events: completed, delta: completedDelta } = await waitForSessionEventReceipt(
        client,
        session.id,
        allowReceiptId,
        BETAS,
        ({ delta: current }) => current.some((event) =>
          event.type === 'agent.tool_result' && event.tool_use_id === awaiting.id)
          && current.some((event) => event.type === 'agent.message')
          && [...current].reverse().find((event) =>
            event.type === 'session.status_idle')?.stop_reason?.type === 'end_turn',
        'P2 original pending ticket to resolve and finish once',
      );
      assert.equal(
        completed.filter((event) =>
          event.type === 'user.tool_confirmation' && event.tool_use_id === awaiting.id).length,
        1,
        'P2/E3 original ticket has one exact confirmation',
      );
      assert.equal(
        completed.filter((event) =>
          event.type === 'agent.tool_result' && event.tool_use_id === awaiting.id).length,
        1,
        'P2/E3 confirmed occurrence has one exact tool result',
      );
      assert.equal(
        completedDelta.filter((event) => event.type === 'agent.message').length,
        1,
        'P2/E3 continuation commits one terminal reply',
      );
      assert.ok(
        !completed.some((event) => event.type === 'user.message'
          && event.content?.some((block) => block.text === 'UNRELATED-WHILE-PENDING')),
        'P2/E3 rejected unrelated User never enters history',
      );
      pass('pending-ticket rejection is atomic and the original ticket still resolves once');
    }

    // 4. tool_confirmation when NOTHING is awaiting (fresh session, no turn).
    {
      const session = await client.beta.sessions.create({
        agent: 'assistant',
        environment_id: 'env_local',
        betas: BETAS,
      });
      const status = await statusOf(
        client.beta.sessions.events.send(session.id, {
          events: [
            { type: 'user.tool_confirmation', tool_use_id: 'toolu_none', result: 'allow' },
          ],
          betas: BETAS,
        }),
        'confirm with nothing awaiting',
      );
      assert.ok(isClientError(status), `confirmation with no pending tool fails closed (got ${status})`);
      pass(`tool_confirmation with nothing awaiting -> fail closed (${status})`);
    }

    // 5. custom_tool_result with no matching outstanding ticket -> fail closed.
    {
      const session = await client.beta.sessions.create({
        agent: 'assistant',
        environment_id: 'env_local',
        betas: BETAS,
      });
      const status = await statusOf(
        client.beta.sessions.events.send(session.id, {
          events: [
            {
              type: 'user.custom_tool_result',
              custom_tool_use_id: 'toolu_no_ticket',
              content: 'result',
            },
          ],
          betas: BETAS,
        }),
        'custom_tool_result with no ticket',
      );
      assert.ok(isClientError(status), `custom_tool_result without a ticket fails closed (got ${status})`);
      pass(`custom_tool_result with no matching ticket -> fail closed (${status})`);
    }

    console.log('E2E PASS: managed error paths fail closed with the right status codes.');
  } finally {
    await stopServer(a.server);
    upstream.close();
  }
}

main().catch((err) => {
  console.error('E2E FAIL:', err);
  process.exitCode = 1;
});
