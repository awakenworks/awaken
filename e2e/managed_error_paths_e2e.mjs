// Managed error-path e2e (ported from awaken-next exception cases): drives the
// managed sessions API through the official @anthropic-ai/sdk and asserts the
// fail-closed / not-found / wrong-ticket rejections a Claude-Managed-Agents server
// must enforce. All deterministic (probe mode). Ported scenarios:
//   - events to an unknown session            -> 404 not_found
//   - retrieve an unknown session             -> 404 not_found
//   - tool_confirmation with a WRONG tool_use_id on a parked run -> fail closed (4xx)
//   - tool_confirmation when nothing is parked -> fail closed (4xx)
//   - custom_tool_result with no matching ticket -> fail closed (4xx)
//
// Run: (from e2e/)  node managed_error_paths_e2e.mjs

import assert from 'node:assert/strict';
import Anthropic from '@anthropic-ai/sdk';
import { spawnServer, stopServer, waitForPort, pass } from './harness.mjs';

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

async function listEvents(client, id) {
  const events = [];
  for await (const ev of client.beta.sessions.events.list(id, { betas: BETAS })) events.push(ev);
  return events;
}

async function main() {
  const a = spawnServer('probe', PORT);
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

    // 3. tool_confirmation with a WRONG tool_use_id on a genuinely parked run: the
    //    run parked on tool X; confirming a different id must fail closed, not
    //    resolve the real pending tool.
    {
      const session = await client.beta.sessions.create({
        agent: 'assistant',
        environment_id: 'env_local',
        betas: BETAS,
      });
      await client.beta.sessions.events.send(session.id, {
        events: [{ type: 'user.message', content: [{ type: 'text', text: 'PARK-ME' }] }],
        betas: BETAS,
      });
      const parked = (await listEvents(client, session.id)).find((e) => e.type === 'agent.tool_use');
      assert.ok(parked, 'the run parked on a tool_use');
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
    }

    // 4. tool_confirmation when NOTHING is parked (fresh session, no turn).
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
        'confirm with nothing parked',
      );
      assert.ok(isClientError(status), `confirmation with no pending tool fails closed (got ${status})`);
      pass(`tool_confirmation with nothing parked -> fail closed (${status})`);
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
  }
}

main().catch((err) => {
  console.error('E2E FAIL:', err);
  process.exitCode = 1;
});
