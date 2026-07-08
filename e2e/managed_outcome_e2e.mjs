// Outcome end-to-end with the official Anthropic TypeScript SDK: `define_outcome`
// drives a grade->revise loop, emitting span.outcome_evaluation_start/end. Covers
// BOTH the satisfied path (revision meets the rubric) and the
// max_iterations_reached path (rubric can never be met within the budget).
//
// Uses the revise server (AWAKEN_MODEL_MODE=revise): reply "a rough draft", then
// "FINAL answer" once it sees feedback; the keyword grader checks the rubric text.
//
// Run: (from e2e/)  npm install && node managed_outcome_e2e.mjs

import assert from 'node:assert/strict';
import Anthropic from '@anthropic-ai/sdk';
import { withRealServer } from './harness.mjs';

const PORT = Number(process.env.E2E_PORT ?? 38103);
const BETAS = ['managed-agents-2026-04-01'];

async function outcomeEnds(client, sessionId) {
  const events = [];
  for await (const ev of client.beta.sessions.events.list(sessionId, { betas: BETAS })) events.push(ev);
  return events.filter((e) => e.type === 'span.outcome_evaluation_end');
}

async function draftThenOutcome(client, rubric, maxIterations) {
  const session = await client.beta.sessions.create({ agent: 'assistant', environment_id: 'env_local', betas: BETAS });
  await client.beta.sessions.events.send(session.id, {
    events: [{ type: 'user.message', content: [{ type: 'text', text: 'write something' }] }],
    betas: BETAS,
  });
  await client.beta.sessions.events.send(session.id, {
    events: [{ type: 'user.define_outcome', description: 'finish it', rubric: { type: 'text', content: rubric }, max_iterations: maxIterations }],
    betas: BETAS,
  });
  return outcomeEnds(client, session.id);
}

async function main() {
  await withRealServer('revise', PORT, async (baseUrl) => {
    const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: baseUrl });

    // Satisfied: the revision contains "FINAL" -> satisfied.
    const satisfied = await draftThenOutcome(client, 'FINAL', 3);
    assert.ok(satisfied.length >= 2, `expected >=2 rounds, got ${satisfied.length}`);
    assert.equal(satisfied[0].result, 'needs_revision');
    assert.equal(satisfied.at(-1).result, 'satisfied');
    console.log('  ok: needs_revision -> satisfied');

    // Unsatisfiable rubric within the budget -> max_iterations_reached.
    const exhausted = await draftThenOutcome(client, 'NEVER_PRESENT_TOKEN', 2);
    assert.equal(exhausted.at(-1).result, 'max_iterations_reached', `results: ${exhausted.map((e) => e.result)}`);
    console.log('  ok: unsatisfiable rubric -> max_iterations_reached');

    console.log('E2E PASS: define_outcome satisfied + max_iterations paths via TS SDK.');
  });
}

main().catch((err) => {
  console.error('E2E FAIL:', err);
  process.exit(1);
});
