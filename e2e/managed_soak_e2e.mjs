// Long-stability (soak) over the session state machine, via the official Anthropic
// TS SDK against awaken-server (echo model).
//
// Drives one session through many running<->idle cycles and asserts the machine
// stays well-formed for the whole run: every turn appends exactly one
// session.status_running, one agent.message (the correct echo, in order), and one
// session.status_idle{end_turn}; event ids are globally unique and stable when reread;
// the session is idle between turns and at the end. Catches leaks/regressions that only
// show up after sustained cycling. IDs need not be numerically monotonic in ledger order:
// streaming preview reserves an agent.message id before the running marker is committed.
// (id reuse, dropped/duplicated status events, drift into a non-idle state).
//
// Tune with SOAK_TURNS (default 150). Run: (from e2e/)  SOAK_TURNS=300 node managed_soak_e2e.mjs

import assert from 'node:assert/strict';
import Anthropic from '@anthropic-ai/sdk';
import { withRealServer, pass } from './harness.mjs';

const BETAS = ['managed-agents-2026-04-01'];
const PORT = Number(process.env.E2E_PORT ?? 38405);
const TURNS = Number(process.env.SOAK_TURNS ?? 150);
const CHECKPOINT = Math.max(1, Math.floor(TURNS / 5));

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

      for (let i = 0; i < TURNS; i++) {
        await client.beta.sessions.events.send(session.id, {
          events: [{ type: 'user.message', content: [{ type: 'text', text: `turn-${i}` }] }],
          betas: BETAS,
        });
        if ((i + 1) % CHECKPOINT === 0) {
          const s = await client.beta.sessions.retrieve(session.id, { betas: BETAS });
          assert.equal(s.status, 'idle', `mid-soak the session is idle after turn ${i}`);
        }
      }

      // The state machine's ledger after the whole run.
      const events = await listAll(client, session.id);
      const runnings = events.filter((e) => e.type === 'session.status_running');
      const messages = events.filter((e) => e.type === 'agent.message');
      const idles = events.filter((e) => e.type === 'session.status_idle');

      assert.equal(runnings.length, TURNS, `exactly one status_running per turn (${runnings.length}/${TURNS})`);
      assert.equal(messages.length, TURNS, `exactly one agent.message per turn (${messages.length}/${TURNS})`);
      assert.equal(idles.length, TURNS, `exactly one status_idle per turn (${idles.length}/${TURNS})`);
      assert.equal(events.length, 3 * TURNS, 'no stray events accumulated');
      for (let i = 0; i < TURNS; i++) {
        assert.deepEqual(
          events.slice(i * 3, i * 3 + 3).map((e) => e.type),
          ['session.status_running', 'agent.message', 'session.status_idle'],
          `turn ${i} kept the running -> message -> idle lifecycle order`,
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
      assert.ok(ids.every((id) => /^evt_\d+$/.test(id)), 'every event id has the stable evt_<sequence> shape');
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
