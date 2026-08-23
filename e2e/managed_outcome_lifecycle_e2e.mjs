// Outcome evaluation LIFECYCLE spans (define-outcomes reference): the grade→revise
// loop must bracket every iteration with a `span.outcome_evaluation_start` and a
// matching `span.outcome_evaluation_end`, carrying a stable `outcome_id` and a
// monotonic `iteration` counter. `managed_outcome_e2e.mjs` asserts only the terminal
// `_end.result`; this covers the start/iteration/pairing contract the reference
// documents but that suite does not check.
//
// Cause/effect graph: C1=the draft Run is committed; C2=the retained Outcome
// reaches terminal committed state after at least two iterations. Effects:
// E1=each iteration projects one start/ongoing/end triple under one outcome_id;
// E2=iterations are zero-based, contiguous, and ordered; E3=the terminal end has
// its decision and explanation. Decision rule L1(C1+C2)->E1+E2+E3. Active or
// errored aggregates project no partial Outcome spans and are owned by recovery.
// Constraints/invariant: one outcome_id owns a contiguous zero-based sequence;
// every start has exactly one ordered end before the terminal decision.
//
// Run: (from e2e/)  node managed_outcome_lifecycle_e2e.mjs

import assert from 'node:assert/strict';
import Anthropic from '@anthropic-ai/sdk';
import { pass, waitForSessionEventReceipt, withRealServer } from './harness.mjs';

const PORT = Number(process.env.E2E_PORT ?? 38432);
const BETAS = ['managed-agents-2026-04-01'];

async function main() {
  await withRealServer('revise', PORT, async (baseUrl) => {
    const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: baseUrl });

    const session = await client.beta.sessions.create({ agent: 'assistant', environment_id: 'env_local', betas: BETAS });
    const draftReceipt = await client.beta.sessions.events.send(session.id, {
      events: [{ type: 'user.message', content: [{ type: 'text', text: 'write something' }] }],
      betas: BETAS,
    });
    // L1 draft rule: C=exact draft receipt; E=a later message and end-turn idle.
    // K=preexisting history cannot settle this Run. R=C+E=>advance to Outcome.
    await waitForSessionEventReceipt(
      client,
      session.id,
      draftReceipt.data[0]?.id,
      BETAS,
      ({ delta }) => delta.some((event) => event.type === 'agent.message')
        && [...delta].reverse().find((event) => event.type === 'session.status_idle')?.stop_reason?.type === 'end_turn',
      'L1 draft Run to commit',
    );
    const outcomeReceipt = await client.beta.sessions.events.send(session.id, {
      events: [{ type: 'user.define_outcome', description: 'finish it', rubric: { type: 'text', content: 'FINAL' }, max_iterations: 3 }],
      betas: BETAS,
    });
    const outcome = outcomeReceipt.data[0];
    // L1 Outcome rule: C=exact DefineOutcome receipt; E=its satisfied terminal
    // span. K=outcome_id and receipt fence the same lifecycle. R=C+E=>inspect.
    const { delta: events } = await waitForSessionEventReceipt(
      client,
      session.id,
      outcome.id,
      BETAS,
      ({ delta }) => delta.some((event) =>
          event.type === 'span.outcome_evaluation_end'
            && event.outcome_id === outcome.outcome_id
            && event.result === 'satisfied'),
      'L1 terminal Outcome projection',
    );
    const starts = events.filter((event) =>
      event.type === 'span.outcome_evaluation_start' && event.outcome_id === outcome.outcome_id);
    const ongoing = events.filter((event) =>
      event.type === 'span.outcome_evaluation_ongoing' && event.outcome_id === outcome.outcome_id);
    const ends = events.filter((event) =>
      event.type === 'span.outcome_evaluation_end' && event.outcome_id === outcome.outcome_id);

    assert.ok(starts.length >= 2, `expected >=2 evaluation rounds, got ${starts.length} starts`);
    pass(`emitted ${starts.length} outcome_evaluation_start spans`);

    // One stable outcome_id across the whole loop.
    const outcomeIds = new Set([...starts, ...ends].map((e) => e.outcome_id));
    assert.equal(outcomeIds.size, 1, `outcome_id should be stable, saw ${[...outcomeIds]}`);
    assert.ok([...outcomeIds][0], 'outcome_id is non-empty');
    pass('single stable outcome_id across start + end spans');

    // Iterations contiguous + ascending from the first start.
    const iters = starts.map((s) => s.iteration);
    const base = iters[0];
    assert.deepEqual(iters, iters.map((_, i) => base + i), `iterations not contiguous ascending: ${iters}`);
    pass(`iteration counter contiguous ascending from ${base}`);

    // Every start pairs with exactly one end at the same iteration (bracketing).
    const endIters = ends.map((e) => e.iteration).sort((a, b) => a - b);
    assert.deepEqual(endIters, [...iters].sort((a, b) => a - b), `start/end iterations unpaired: starts=${iters} ends=${endIters}`);
    assert.deepEqual(
      ongoing.map((event) => event.iteration),
      iters,
      'L1/E1 every start also owns one ongoing projection before its end',
    );
    pass('each start iteration has a matching end (spans bracket every round)');

    // Terminal end carries result + a non-empty explanation.
    const last = ends.at(-1);
    assert.ok(['satisfied', 'max_iterations_reached', 'failed', 'needs_revision'].includes(last.result), `result: ${last.result}`);
    assert.equal(last.result, 'satisfied', 'FINAL rubric should end satisfied');
    assert.ok(typeof last.explanation === 'string' && last.explanation.length > 0, 'terminal end has a non-empty explanation');
    pass('terminal end: satisfied + non-empty explanation');

    console.log('E2E PASS: outcome evaluation lifecycle spans (start/end pairing, stable outcome_id, monotonic iteration).');
  });
}

main().catch((err) => {
  console.error('E2E FAIL:', err);
  process.exit(1);
});
