// High-concurrency stress over the session state machine, via the official Anthropic
// TS SDK against awaken-server (echo model).
//
// Runs many independent sessions in parallel — each its own running<->idle state
// machine — and asserts strict ISOLATION and completeness under contention: every
// session sees exactly its own K echoes in order and nothing from any sibling, each
// ends idle, and no request errors. Catches cross-session state bleed, lost/dropped
// Runs under load, and shared-counter races.
//
// Tune with CONC_SESSIONS (default 40) and CONC_RUNS (default 4).
// Run: (from e2e/)  CONC_SESSIONS=80 CONC_RUNS=6 node managed_concurrency_e2e.mjs

import assert from 'node:assert/strict';
import Anthropic from '@anthropic-ai/sdk';
import { waitForSessionEventReceipt, withRealServer, pass } from './harness.mjs';

const BETAS = ['managed-agents-2026-04-01'];
const PORT = Number(process.env.E2E_PORT ?? 38406);
const SESSIONS = Number(process.env.CONC_SESSIONS ?? 40);
const RUNS = Number(process.env.CONC_RUNS ?? 4);

async function runOne(client, idx) {
  const marker = `s${idx}`;
  const session = await client.beta.sessions.create({
    agent: 'assistant',
    environment_id: 'env_local',
    metadata: { marker },
    betas: BETAS,
  });
  // Cause/effect graph: C1=distinct Sessions execute concurrently; C2=each
  // Session admits K inputs sequentially after the preceding Run settles;
  // C3=each input has a Session-unique marker. Effects: E1=all Session ids are
  // unique; E2=each history has exactly K ordered echoes and idle boundaries;
  // E3=no sibling marker crosses the Session boundary. Decision rule
  // K1(C1+C2+C3)->E1+E2+E3. Waiting after each send preserves the intended K
  // Runs; queued-while-awaiting behavior has its own event-batch owner.
  for (let step = 0; step < RUNS; step++) {
    const receipt = await client.beta.sessions.events.send(session.id, {
      events: [{ type: 'user.message', content: [{ type: 'text', text: `${marker}-${step}` }] }],
      betas: BETAS,
    });
    const receiptId = receipt.data[0]?.id;
    assert.equal(typeof receiptId, 'string', `K1 ${marker} Run ${step + 1} exact receipt`);
    await waitForSessionEventReceipt(
      client,
      session.id,
      receiptId,
      BETAS,
      ({ events, delta }) => delta.some((event) => event.type === 'agent.message')
        && delta.some((event) => event.type === 'session.status_idle')
        && events.filter((event) => event.type === 'agent.message').length === step + 1
        && events.filter((event) => event.type === 'session.status_idle').length === step + 1,
      `K1 ${marker} Run ${step + 1} to settle`,
    );
  }
  const events = [];
  for await (const ev of client.beta.sessions.events.list(session.id, { betas: BETAS })) events.push(ev);
  const echoes = events.filter((e) => e.type === 'agent.message').map((e) => e.content?.[0]?.text);
  const status = (await client.beta.sessions.retrieve(session.id, { betas: BETAS })).status;
  return { idx, marker, sessionId: session.id, echoes, idles: events.filter((e) => e.type === 'session.status_idle').length, status };
}

async function main() {
  // Test design (contention matrix). Causes: C1=SESSIONS distinct Sessions run
  // concurrently; C2=each receives RUNS ordered unique markers. Effects:
  // E1=all ids remain distinct; E2=each Session commits exactly its own ordered
  // replies/idles and ends idle; E3=no marker crosses Session boundaries.
  // Constraints/invariant: the Session repository is the sole isolation owner
  // and completion is observed from committed history, not task completion alone.
  // Decision rule: Q1=C1+C2=>E1+E2+E3 for every matrix cell; any missing,
  // duplicate, reordered, or foreign marker fails the whole stress test.
  try {
    await withRealServer('echo', PORT, async (baseUrl) => {
      const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: baseUrl });
      pass(`stressing ${SESSIONS} concurrent Sessions x ${RUNS} Runs`);

      const results = await Promise.all(
        Array.from({ length: SESSIONS }, (_, idx) => runOne(client, idx)),
      );

      const sessionIds = new Set(results.map((r) => r.sessionId));
      assert.equal(sessionIds.size, SESSIONS, 'every concurrent create returned a distinct session');

      for (const r of results) {
        const expected = Array.from({ length: RUNS }, (_, step) => `Echo: ${r.marker}-${step}`);
        // Isolation + order + completeness: this Session sees exactly its own Runs.
        assert.deepEqual(
          r.echoes,
          expected,
          `Session ${r.marker} sees exactly its own ${RUNS} echoes in order (got ${JSON.stringify(r.echoes)})`,
        );
        // No sibling's marker leaked in.
        assert.ok(
          r.echoes.every((e) => e.startsWith(`Echo: ${r.marker}-`)),
          `no cross-session bleed into ${r.marker}`,
        );
        assert.equal(r.idles, RUNS, `Session ${r.marker} closed each Run with a status_idle`);
        assert.equal(r.status, 'idle', `Session ${r.marker} is idle at rest`);
      }
      pass(`${SESSIONS}x${RUNS} = ${SESSIONS * RUNS} Runs under contention: full isolation, in order, all idle`);
    });

    console.log(`E2E PASS: ${SESSIONS} concurrent session state machines isolated + complete via TS SDK.`);
    process.exitCode = 0;
  } catch (err) {
    console.error('E2E FAIL:', err);
    process.exitCode = 1;
  }
}

main();
