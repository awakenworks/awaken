// Long-stability (soak) over the session state machine, via the official Anthropic
// TS SDK against awaken-server (echo model).
//
// Drives one session through many running<->idle cycles and asserts the machine
// stays well-formed for the whole run: every turn appends the exact Managed
// aggregate + primary-Thread + model-span + reply + usage lifecycle; event ids
// are globally unique and stable when reread;
// the session is idle between turns and at the end. Catches leaks/regressions that only
// show up after sustained cycling. IDs need not be numerically monotonic in ledger order:
// streaming preview reserves an agent.message id before the running marker is committed.
// (id reuse, dropped/duplicated status events, drift into a non-idle state).
//
// Tune with SOAK_TURNS (default 150). Run: (from e2e/)  SOAK_TURNS=300 node managed_soak_e2e.mjs

import assert from 'node:assert/strict';
import Anthropic from '@anthropic-ai/sdk';
import { pass, waitForSessionEventReceipt, withRealServer } from './harness.mjs';

const BETAS = ['managed-agents-2026-04-01'];
const PORT = Number(process.env.E2E_PORT ?? 38405);
const TURNS = Number(process.env.SOAK_TURNS ?? 150);
const CHECKPOINT = Math.max(1, Math.floor(TURNS / 5));
const TURN_EVENT_TYPES = [
  'user.message',
  'session.status_running',
  'session.thread_status_running',
  'span.model_request_start',
  'span.model_request_end',
  'agent.message',
  'session.thread_status_idle',
  'session.usage',
  'session.status_idle',
];

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
      pass(`soaking ${TURNS} turns on ${session.id}`);

      // Per-turn decision S1: C1 exact turn receipt; E1 processed receipt with
      // its matching echo and end_turn idle. K1 an earlier turn's echo/idle cannot
      // satisfy a later cycle. D1=C1=>E1, repeated TURNS times before aggregate
      // ledger and stable-reread effects are checked below.
      for (let i = 0; i < TURNS; i++) {
        const receipt = (await client.beta.sessions.events.send(session.id, {
          events: [{ type: 'user.message', content: [{ type: 'text', text: `turn-${i}` }] }],
          betas: BETAS,
        })).data[0];
        await waitForSessionEventReceipt(
          client,
          session.id,
          receipt.id,
          BETAS,
          ({ delta }) => delta.some((event) => event.type === 'agent.message'
            && event.content?.[0]?.text === `Echo: turn-${i}`)
            && delta.some((event) => event.type === 'session.status_idle'
              && event.stop_reason?.type === 'end_turn'),
          `soak turn ${i} lifecycle`,
        );
        if ((i + 1) % CHECKPOINT === 0) {
          const s = await client.beta.sessions.retrieve(session.id, { betas: BETAS });
          assert.equal(s.status, 'idle', `mid-soak the session is idle after turn ${i}`);
        }
      }

      // The state machine's ledger after the whole run.
      const events = await listAll(client, session.id);
      const inputs = events.filter((e) => e.type === 'user.message');
      const runnings = events.filter((e) => e.type === 'session.status_running');
      const threadRunnings = events.filter((e) => e.type === 'session.thread_status_running');
      const requestStarts = events.filter((e) => e.type === 'span.model_request_start');
      const requestEnds = events.filter((e) => e.type === 'span.model_request_end');
      const messages = events.filter((e) => e.type === 'agent.message');
      const threadIdles = events.filter((e) => e.type === 'session.thread_status_idle');
      const usages = events.filter((e) => e.type === 'session.usage');
      const idles = events.filter((e) => e.type === 'session.status_idle');

      assert.equal(inputs.length, TURNS, `exactly one user.message per turn (${inputs.length}/${TURNS})`);
      assert.equal(runnings.length, TURNS, `exactly one status_running per turn (${runnings.length}/${TURNS})`);
      assert.equal(threadRunnings.length, TURNS, 'exactly one primary Thread running edge per turn');
      assert.equal(requestStarts.length, TURNS, 'exactly one model-request start per turn');
      assert.equal(requestEnds.length, TURNS, 'exactly one model-request end per turn');
      assert.equal(messages.length, TURNS, `exactly one agent.message per turn (${messages.length}/${TURNS})`);
      assert.equal(threadIdles.length, TURNS, 'exactly one primary Thread idle edge per turn');
      assert.equal(usages.length, TURNS, 'exactly one cumulative usage snapshot per turn');
      assert.equal(idles.length, TURNS, `exactly one status_idle per turn (${idles.length}/${TURNS})`);
      assert.equal(events.length, TURN_EVENT_TYPES.length * TURNS, 'no stray events accumulated');
      for (let i = 0; i < TURNS; i++) {
        // Cause/effect decision rule per turn: C1 one admitted input; effects E1
        // aggregate/Thread running, E2 paired model span, E3 reply, E4 Thread
        // idle, E5 usage, E6 aggregate idle. Constraint: no cross-turn event may
        // enter this nine-event slice. D1=C1=>E1-E6 in exact order.
        assert.deepEqual(
          events
            .slice(i * TURN_EVENT_TYPES.length, (i + 1) * TURN_EVENT_TYPES.length)
            .map((e) => e.type),
          TURN_EVENT_TYPES,
          `turn ${i} kept the exact aggregate/Thread/span/message/usage lifecycle order`,
        );
      }

      // Echoes are in order — the machine never reordered or dropped a turn.
      for (let i = 0; i < TURNS; i++) {
        assert.equal(messages[i].content?.[0]?.text, `Echo: turn-${i}`, `turn ${i} echoed in order`);
      }
      // Every idle is a clean end_turn.
      assert.ok(idles.every((e) => e.stop_reason.type === 'end_turn'), 'every turn ended with end_turn');

      // IDs are globally unique and stable across a second read. A streaming preview
      // reserves each message id before the surrounding lifecycle markers, so numeric
      // ordering is deliberately not a ledger-order invariant.
      const ids = events.map((e) => e.id);
      assert.equal(new Set(ids).size, ids.length, 'no event id was reused across the soak');
      assert.ok(
        ids.every((id) => typeof id === 'string' && id.length > 0),
        'every public event id is a nonempty opaque string',
      );
      assert.deepEqual((await listAll(client, session.id)).map((e) => e.id), ids, 'event ids are stable when reread');

      const finalStatus = (await client.beta.sessions.retrieve(session.id, { betas: BETAS })).status;
      assert.equal(finalStatus, 'idle', 'the session is idle at the end of the soak');
      pass(`${TURNS} running<->idle cycles: ordered echoes, unique+stable ids, clean end_turn, idle at rest`);
    });

    console.log(`E2E PASS: session state machine stable across ${TURNS} turns (soak) via TS SDK.`);
    process.exitCode = 0;
  } catch (err) {
    console.error('E2E FAIL:', err);
    process.exitCode = 1;
  }
}

main();
