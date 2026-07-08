// Long-stability (soak) over the session state machine, via the official Anthropic
// TS SDK against awaken-server-local (echo model).
//
// Drives one session through many running<->idle cycles and asserts the machine
// stays well-formed for the whole run: every turn appends exactly one agent.message
// (the correct echo, in order) and one session.status_idle{end_turn}; event ids are
// globally unique and strictly monotonic; the session is idle between turns and at
// the end. Catches leaks/regressions that only show up after sustained cycling
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
      const messages = events.filter((e) => e.type === 'agent.message');
      const idles = events.filter((e) => e.type === 'session.status_idle');

      assert.equal(messages.length, TURNS, `exactly one agent.message per turn (${messages.length}/${TURNS})`);
      assert.equal(idles.length, TURNS, `exactly one status_idle per turn (${idles.length}/${TURNS})`);
      assert.equal(events.length, 2 * TURNS, 'no stray events accumulated');

      // Echoes are in order — the machine never reordered or dropped a turn.
      for (let i = 0; i < TURNS; i++) {
        assert.equal(messages[i].content?.[0]?.text, `Echo: turn-${i}`, `turn ${i} echoed in order`);
      }
      // Every idle is a clean end_turn.
      assert.ok(idles.every((e) => e.stop_reason.type === 'end_turn'), 'every turn ended with end_turn');

      // Ids are globally unique and strictly monotonic across the whole soak.
      const ids = events.map((e) => e.id);
      assert.equal(new Set(ids).size, ids.length, 'no event id was reused across the soak');
      const nums = ids.map((id) => Number(id.replace(/\D/g, '')));
      for (let i = 1; i < nums.length; i++) {
        assert.ok(nums[i] > nums[i - 1], `event ids are strictly increasing (${ids[i - 1]} -> ${ids[i]})`);
      }

      const finalStatus = (await client.beta.sessions.retrieve(session.id, { betas: BETAS })).status;
      assert.equal(finalStatus, 'idle', 'the session is idle at the end of the soak');
      pass(`${TURNS} running<->idle cycles: ordered echoes, unique+monotonic ids, clean end_turn, idle at rest`);
    });

    console.log(`E2E PASS: session state machine stable across ${TURNS} turns (soak) via TS SDK.`);
    process.exitCode = 0;
  } catch (err) {
    console.error('E2E FAIL:', err);
    process.exitCode = 1;
  }
}

main();
