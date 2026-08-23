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

const PORT = Number(process.env.E2E_PORT ?? 38103);
const BETAS = ['managed-agents-2026-04-01'];
const STORE_DIR = `/tmp/awaken-outcome-e2e-${process.pid}`;

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
      const satisfiedSession = await client.beta.sessions.retrieve(satisfiedRun.sessionId, { betas: BETAS });
      assert.equal(satisfiedSession.outcome_evaluations.length, 1);
      assert.deepEqual(satisfiedSession.outcome_evaluations[0], {
        completed_at: '2026-01-01T00:00:00Z',
        description: 'finish it',
        explanation: satisfied.at(-1).explanation,
        iteration: satisfied.at(-1).iteration,
        outcome_id: satisfiedRun.outcomeId,
        result: 'satisfied',
        type: 'outcome_evaluation',
      });
      console.log('  ok: needs_revision -> satisfied');

      // Unsatisfiable rubric within the budget -> max_iterations_reached.
      const exhaustedRun = await draftThenOutcome(client, 'NEVER_PRESENT_TOKEN', 2);
      const exhausted = exhaustedRun.ends;
      assert.equal(exhausted.at(-1).result, 'max_iterations_reached', `results: ${exhausted.map((e) => e.result)}`);
      const exhaustedSession = await client.beta.sessions.retrieve(exhaustedRun.sessionId, { betas: BETAS });
      assert.equal(exhaustedSession.outcome_evaluations.length, 1);
      assert.equal(exhaustedSession.outcome_evaluations[0].outcome_id, exhaustedRun.outcomeId);
      assert.equal(exhaustedSession.outcome_evaluations[0].result, 'max_iterations_reached');
      assert.ok(exhaustedSession.outcome_evaluations[0].completed_at);
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
