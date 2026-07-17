// Outcome evaluation LIFECYCLE spans (define-outcomes reference): the grade→revise
// loop must bracket every iteration with a `span.outcome_evaluation_start` and a
// matching `span.outcome_evaluation_end`, carrying a stable `outcome_id` and a
// monotonic `iteration` counter. `managed_outcome_e2e.mjs` asserts only the terminal
// `_end.result`; this covers the start/iteration/pairing contract the reference
// documents but that suite does not check.
//
// Design: state-transition over the per-iteration span pair — every `_start`
// (outcome_id, iteration) has exactly one `_end` at the same (outcome_id, iteration);
// iterations are contiguous and ascending; the terminal `_end` carries a non-empty
// explanation. Uses the revise server (reply a draft, then "FINAL" once feedback
// appears) so the loop runs at least two iterations deterministically.
//
// Run: (from e2e/)  node managed_outcome_lifecycle_e2e.mjs

import assert from 'node:assert/strict';
import Anthropic from '@anthropic-ai/sdk';
import { withRealServer, pass } from './harness.mjs';

const PORT = Number(process.env.E2E_PORT ?? 38432);
const BETAS = ['managed-agents-2026-04-01'];

async function main() {
  await withRealServer('revise', PORT, async (baseUrl) => {
    const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: baseUrl });

    const session = await client.beta.sessions.create({ agent: 'assistant', environment_id: 'env_local', betas: BETAS });
    await client.beta.sessions.events.send(session.id, {
      events: [{ type: 'user.message', content: [{ type: 'text', text: 'write something' }] }],
      betas: BETAS,
    });
    await client.beta.sessions.events.send(session.id, {
      events: [{ type: 'user.define_outcome', description: 'finish it', rubric: { type: 'text', content: 'FINAL' }, max_iterations: 3 }],
      betas: BETAS,
    });

    const events = [];
    for await (const ev of client.beta.sessions.events.list(session.id, { betas: BETAS })) events.push(ev);
    const starts = events.filter((e) => e.type === 'span.outcome_evaluation_start');
    const ends = events.filter((e) => e.type === 'span.outcome_evaluation_end');

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
