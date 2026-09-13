// Outcome end-to-end with the official Anthropic TypeScript SDK: `define_outcome`
// drives a grade->revise loop, emitting span.outcome_evaluation_start/end. Covers
// BOTH the satisfied path (revision meets the rubric) and the
// max_iterations_reached path (rubric can never be met within the budget).
//
// Uses the revise server (AWAKEN_MODEL_MODE=revise): reply "a rough draft", then
// "FINAL answer" once it sees feedback; the keyword grader checks the rubric text.
//
// Run: (from e2e/)  npm install && node managed_outcome_e2e.mjs
//
// Cause/effect graph: C1=the draft User Run has committed; C2=one retained
// define_outcome command owns a stable outcome_id; C3=the rubric is satisfied
// after revision; C4=the iteration cap is reached first. Effects: E1=the HTTP
// receipt may precede execution but its same id becomes processed; E2=terminal
// Outcome state projects one durable, idempotent span sequence; E3=Session owns
// exactly one terminal evaluation for the outcome_id. Decision table:
// | Rule | Draft committed | rubric reached | Effect |
// | O1 | yes | after revise | E1-E3, satisfied |
// | O2 | yes | no at cap | E1-E3, max_iterations_reached |
// Constraints/invariant: one retained definition owns one outcome_id and one
// ordered terminal evaluation; HTTP acceptance alone never proves completion.

import assert from 'node:assert/strict';
import fs from 'node:fs';
import Anthropic from '@anthropic-ai/sdk';
import { waitForSessionEventReceipt, withRealServer } from './harness.mjs';
import { FAKE_USAGE } from './fixtures/fake_anthropic_fixture.mjs';

const PORT = Number(process.env.E2E_PORT ?? 38103);
const BETAS = ['managed-agents-2026-04-01'];
const STORE_DIR = `/tmp/awaken-outcome-e2e-${process.pid}`;

async function assertTerminalEvaluation(client, run, result) {
  // O3/O4 cause/effect: satisfied OR budget-exhausted terminal evaluation has
  // one owning commit. Its public span processed_at and Session completed_at
  // must agree, be valid RFC3339, and survive repeated reads unchanged. The
  // production projection test supplies distinct nonmonotonic commit clocks
  // and checks exact ownership; this SDK seam never invents another clock.
  const end = run.ends.at(-1);
  assert.equal(typeof end.processed_at, 'string');
  assert.match(end.processed_at, /^\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d(?:\.\d+)?Z$/u);
  assert.ok(Number.isFinite(Date.parse(end.processed_at)));
  const session = await client.beta.sessions.retrieve(run.sessionId, { betas: BETAS });
  assert.deepEqual(session.outcome_evaluations, [{
    completed_at: end.processed_at,
    description: 'finish it',
    explanation: end.explanation,
    iteration: end.iteration,
    outcome_id: run.outcomeId,
    result,
    type: 'outcome_evaluation',
  }]);
  const replay = await client.beta.sessions.retrieve(run.sessionId, { betas: BETAS });
  assert.deepEqual(replay.outcome_evaluations, session.outcome_evaluations, 'O4 reads cannot rewrite completion');
}

async function draftThenOutcome(client, rubric, maxIterations) {
  const session = await client.beta.sessions.create({ agent: 'assistant', environment_id: 'env_local', betas: BETAS });
  const draftReceipt = await client.beta.sessions.events.send(session.id, {
    events: [{ type: 'user.message', content: [{ type: 'text', text: 'write something' }] }],
    betas: BETAS,
  });
  const draftReceiptId = draftReceipt.data[0]?.id;
  assert.equal(typeof draftReceiptId, 'string', 'draft Run returns its exact User Event receipt');
  await waitForSessionEventReceipt(
    client,
    session.id,
    draftReceiptId,
    BETAS,
    ({ delta }) => delta.some((event) => event.type === 'agent.message')
      && [...delta].reverse().find((event) => event.type === 'session.status_idle')?.stop_reason?.type === 'end_turn',
    'the draft User Run to commit before defining its Outcome',
  );
  const outcomeReceipt = await client.beta.sessions.events.send(session.id, {
    events: [{ type: 'user.define_outcome', description: 'finish it', rubric: { type: 'text', content: rubric }, max_iterations: maxIterations }],
    betas: BETAS,
  });
  const accepted = outcomeReceipt.data[0];
  assert.equal(accepted?.type, 'user.define_outcome');
  assert.match(accepted.outcome_id, /^outc_/u, 'the retained command owns one public Outcome id');
  const { events: terminalEvents } = await waitForSessionEventReceipt(
    client,
    session.id,
    accepted.id,
    BETAS,
    ({ delta }) => delta.some((event) =>
        event.type === 'span.outcome_evaluation_end'
          && event.outcome_id === accepted.outcome_id
          && event.result !== 'needs_revision'),
    'the durable Outcome report to reach a terminal projection',
  );
  return {
    sessionId: session.id,
    outcomeId: accepted.outcome_id,
    ends: terminalEvents.filter((event) =>
      event.type === 'span.outcome_evaluation_end' && event.outcome_id === accepted.outcome_id),
  };
}

async function main() {
  fs.rmSync(STORE_DIR, { recursive: true, force: true });
  try {
    await withRealServer('revise', PORT, async (baseUrl) => {
      const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: baseUrl });

      // Satisfied: the revision contains "FINAL" -> satisfied.
      const satisfiedRun = await draftThenOutcome(client, 'FINAL', 3);
      const satisfied = satisfiedRun.ends;
      assert.ok(satisfied.length >= 2, `expected >=2 rounds, got ${satisfied.length}`);
      assert.equal(satisfied[0].result, 'needs_revision');
      assert.equal(satisfied.at(-1).result, 'satisfied');
      // Each evaluation span reports what its own Judge Run cost, read from that
      // Run's committed Thread usage cell. One grading request per round, so the
      // counters are exactly one FAKE_USAGE reply: the provider reports prompt
      // tokens inclusive of the cache breakdown, so input is 11 + 3 + 2. A
      // defaulted or omitted field would show as zeroes or a missing key, and
      // the SDK declares this one required.
      const judgeCost = {
        input_tokens: FAKE_USAGE.input_tokens
          + FAKE_USAGE.cache_read_input_tokens
          + FAKE_USAGE.cache_creation_input_tokens,
        output_tokens: FAKE_USAGE.output_tokens,
        cache_read_input_tokens: FAKE_USAGE.cache_read_input_tokens,
        cache_creation_input_tokens: FAKE_USAGE.cache_creation_input_tokens,
      };
      for (const end of satisfied) {
        assert.deepEqual(end.usage, judgeCost,
          `evaluation ${end.iteration} must report the Judge Run's own cost`);
      }
      await assertTerminalEvaluation(client, satisfiedRun, 'satisfied');
      console.log('  ok: needs_revision -> satisfied, each round reports its Judge cost');

      // Unsatisfiable rubric within the budget -> max_iterations_reached.
      const exhaustedRun = await draftThenOutcome(client, 'NEVER_PRESENT_TOKEN', 2);
      const exhausted = exhaustedRun.ends;
      assert.equal(exhausted.at(-1).result, 'max_iterations_reached', `results: ${exhausted.map((e) => e.result)}`);
      await assertTerminalEvaluation(client, exhaustedRun, 'max_iterations_reached');
      console.log('  ok: unsatisfiable rubric -> max_iterations_reached');

      console.log('E2E PASS: define_outcome satisfied + max_iterations paths via TS SDK.');
    }, { extraEnv: { SESSION_DEPLOYMENT_STORAGE_DIR: STORE_DIR } });
  } finally {
    fs.rmSync(STORE_DIR, { recursive: true, force: true });
  }
}

main().catch((err) => {
  console.error('E2E FAIL:', err);
  process.exit(1);
});
