// High-concurrency stress over the session state machine, via the official Anthropic
// TS SDK against awaken-server-local (echo model).
//
// Runs many independent sessions in parallel — each its own running<->idle state
// machine — and asserts strict ISOLATION and completeness under contention: every
// session sees exactly its own K echoes in order and nothing from any sibling, each
// ends idle, and no request errors. Catches cross-session state bleed, lost/dropped
// turns under load, and shared-counter races.
//
// Tune with CONC_SESSIONS (default 40) and CONC_TURNS (default 4).
// Run: (from e2e/)  CONC_SESSIONS=80 CONC_TURNS=6 node managed_concurrency_e2e.mjs

import assert from 'node:assert/strict';
import Anthropic from '@anthropic-ai/sdk';
import { withRealServer, pass } from './harness.mjs';

const BETAS = ['managed-agents-2026-04-01'];
const PORT = Number(process.env.E2E_PORT ?? 38406);
const SESSIONS = Number(process.env.CONC_SESSIONS ?? 40);
const TURNS = Number(process.env.CONC_TURNS ?? 4);

async function runOne(client, idx) {
  const marker = `s${idx}`;
  const session = await client.beta.sessions.create({
    agent: 'assistant',
    environment_id: 'env_local',
    metadata: { marker },
    betas: BETAS,
  });
  // K sequential turns on THIS session (each drives running -> idle).
  for (let t = 0; t < TURNS; t++) {
    await client.beta.sessions.events.send(session.id, {
      events: [{ type: 'user.message', content: [{ type: 'text', text: `${marker}-${t}` }] }],
      betas: BETAS,
    });
  }
  const events = [];
  for await (const ev of client.beta.sessions.events.list(session.id, { betas: BETAS })) events.push(ev);
  const echoes = events.filter((e) => e.type === 'agent.message').map((e) => e.content?.[0]?.text);
  const status = (await client.beta.sessions.retrieve(session.id, { betas: BETAS })).status;
  return { idx, marker, sessionId: session.id, echoes, idles: events.filter((e) => e.type === 'session.status_idle').length, status };
}

async function main() {
  try {
    await withRealServer('echo', PORT, async (baseUrl) => {
      const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: baseUrl });
      pass(`stressing ${SESSIONS} concurrent sessions x ${TURNS} turns`);

      const results = await Promise.all(
        Array.from({ length: SESSIONS }, (_, idx) => runOne(client, idx)),
      );

      const sessionIds = new Set(results.map((r) => r.sessionId));
      assert.equal(sessionIds.size, SESSIONS, 'every concurrent create returned a distinct session');

      for (const r of results) {
        const expected = Array.from({ length: TURNS }, (_, t) => `Echo: ${r.marker}-${t}`);
        // Isolation + order + completeness: this session sees exactly its own turns.
        assert.deepEqual(
          r.echoes,
          expected,
          `session ${r.marker} sees exactly its own ${TURNS} echoes in order (got ${JSON.stringify(r.echoes)})`,
        );
        // No sibling's marker leaked in.
        assert.ok(
          r.echoes.every((e) => e.startsWith(`Echo: ${r.marker}-`)),
          `no cross-session bleed into ${r.marker}`,
        );
        assert.equal(r.idles, TURNS, `session ${r.marker} closed each turn with a status_idle`);
        assert.equal(r.status, 'idle', `session ${r.marker} is idle at rest`);
      }
      pass(`${SESSIONS}x${TURNS} = ${SESSIONS * TURNS} turns under contention: full isolation, in order, all idle`);
    });

    console.log(`E2E PASS: ${SESSIONS} concurrent session state machines isolated + complete via TS SDK.`);
    process.exitCode = 0;
  } catch (err) {
    console.error('E2E FAIL:', err);
    process.exitCode = 1;
  }
}

main();
