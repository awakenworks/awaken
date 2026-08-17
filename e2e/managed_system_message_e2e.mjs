// §4 — the mid-session system.message inbound event, via the official Anthropic TS
// SDK against awaken-server (echo model).
//
// system.message is the operator channel for changing the system prompt between
// turns. This locks the inbound path: it is accepted (a receipt with processed_at:
// null, no error), it does NOT change the session's status or emit an agent turn on
// its own, and the session keeps working — a following user.message still runs
// normally. The requires_action ordering partitions live in the existing Rust HITL
// decision table, where a system event must trail the resolving tool result.
//
// Run: (from e2e/)  node managed_system_message_e2e.mjs

import assert from 'node:assert/strict';
import Anthropic from '@anthropic-ai/sdk';
import { withRealServer, pass } from './harness.mjs';

const BETAS = ['managed-agents-2026-04-01'];
const PORT = Number(process.env.E2E_PORT ?? 38404);

async function sendUser(client, sid, text) {
  await client.beta.sessions.events.send(sid, {
    events: [{ type: 'user.message', content: [{ type: 'text', text }] }],
    betas: BETAS,
  });
}
async function listTypes(client, sid) {
  const evs = [];
  for await (const e of client.beta.sessions.events.list(sid, { betas: BETAS })) evs.push(e);
  return evs;
}

async function waitFor(client, sid, predicate, description) {
  const deadline = Date.now() + 10_000;
  do {
    const events = await listTypes(client, sid);
    if (predicate(events)) return events;
    await new Promise((resolve) => setTimeout(resolve, 25));
  } while (Date.now() < deadline);
  throw new Error(`timed out waiting for ${description}`);
}

async function main() {
  try {
    await withRealServer('echo', PORT, async (baseUrl) => {
      const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: baseUrl });
      const session = await client.beta.sessions.create({ agent: 'assistant', environment_id: 'env_local', betas: BETAS });

      // A first real turn, so the session has a user turn to follow.
      await sendUser(client, session.id, 'first');
      let events = await waitFor(
        client,
        session.id,
        (listed) => listed.some((event) => event.type === 'agent.message')
          && listed.some((event) => event.type === 'session.status_idle'),
        'the first turn to become idle',
      );
      const beforeCount = events.length;
      const beforeIds = new Set(events.map((event) => event.id));
      assert.ok(events.some((e) => e.type === 'agent.message'), 'first turn produced an agent.message');

      // system.message is accepted and acknowledged with processed_at: null.
      const receipt = await client.beta.sessions.events.send(session.id, {
        events: [{ type: 'system.message', content: [{ type: 'text', text: 'Be terse from now on.' }] }],
        betas: BETAS,
      });
      assert.equal(receipt.data.length, 1);
      assert.equal(receipt.data[0].type, 'system.message', 'the receipt echoes system.message');
      assert.equal(receipt.data[0].processed_at, null, 'system.message is queued (processed_at: null)');
      pass('system.message accepted + acknowledged (receipt, no error)');

      // It persists one same-id inbound event and the normal per-request usage
      // snapshot, emits no agent/status event of its own, and leaves the
      // session idle.
      events = await waitFor(
        client,
        session.id,
        (listed) => listed.some((event) => event.id === receipt.data[0].id && event.processed_at),
        'the persisted system.message acknowledgement',
      );
      assert.equal(
        events.length,
        beforeCount + 2,
        'system.message adds its persisted inbound event and one usage snapshot',
      );
      const added = events.filter((event) => !beforeIds.has(event.id));
      assert.deepEqual(
        added.map((event) => event.type).sort(),
        ['session.usage', 'system.message'],
        'system.message emits neither an agent turn nor a status transition',
      );
      const persistedSystem = events.find((event) => event.type === 'system.message');
      assert.equal(persistedSystem?.id, receipt.data[0].id, 'receipt and history share system id');
      assert.ok(persistedSystem?.processed_at, 'persisted system event is processed');
      const status = (await client.beta.sessions.retrieve(session.id, { betas: BETAS })).status;
      assert.equal(status, 'idle', 'the session stays idle after a system.message');
      pass('system.message emits no agent turn and keeps the session idle');

      // The session keeps working: a following user.message still runs.
      await sendUser(client, session.id, 'second');
      events = await listTypes(client, session.id);
      const echoes = events.filter((e) => e.type === 'agent.message').map((e) => e.content?.[0]?.text);
      assert.ok(echoes.includes('Echo: second'), `next turn still runs (echoes: ${JSON.stringify(echoes)})`);
      pass('a following user.message still runs after the system.message');
    });

    console.log('E2E PASS: mid-session system.message inbound path holds via TS SDK.');
    process.exitCode = 0;
  } catch (err) {
    console.error('E2E FAIL:', err);
    process.exitCode = 1;
  }
}

main();
