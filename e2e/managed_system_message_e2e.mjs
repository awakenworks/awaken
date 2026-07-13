// §4 — the mid-session system.message inbound event, via the official Anthropic TS
// SDK against awaken-server (echo model).
//
// system.message is the operator channel for changing the system prompt between
// turns. This locks the inbound path: it is accepted (a receipt with processed_at:
// null, no error), it does NOT change the session's status or emit an agent turn on
// its own, and the session keeps working — a following user.message still runs
// normally. It is a mid-conversation event, so it is rejected before the first user
// turn (must follow a user message).
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

async function main() {
  try {
    await withRealServer('echo', PORT, async (baseUrl) => {
      const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: baseUrl });
      const session = await client.beta.sessions.create({ agent: 'assistant', environment_id: 'env_local', betas: BETAS });

      // A first real turn, so the session has a user turn to follow.
      await sendUser(client, session.id, 'first');
      let events = await listTypes(client, session.id);
      const beforeCount = events.length;
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

      // It emits no agent turn of its own and leaves the session idle.
      events = await listTypes(client, session.id);
      assert.equal(
        events.length,
        beforeCount,
        'system.message adds no outbound agent/status event on its own',
      );
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
