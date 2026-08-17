// Exhaustive finite-state check for the evaluation oracle. For all expected /
// observed subsets up to four facts, ratios stay bounded, duplicate output is
// idempotent, and F1 is exactly the harmonic mean. This formally checks the
// scorer; stochastic model quality itself is intentionally not "proved".

import assert from 'node:assert/strict';
import { taggedFactMetrics } from './llm_eval_metrics.mjs';

const universe = ['AWKFACT_A', 'AWKFACT_B', 'AWKFACT_C', 'AWKFACT_D'];
const subsets = (items) => Array.from({ length: 1 << items.length }, (_, mask) => (
  items.filter((_, index) => mask & (1 << index))
));

let states = 0;
for (const expected of subsets(universe)) {
  for (const observed of subsets(universe)) {
    const text = observed.join(' ');
    const score = taggedFactMetrics({ expected, text });
    for (const metric of ['precision', 'recall', 'f1', 'contamination_rate', 'stale_fact_rate']) {
      assert.ok(score[metric] >= 0 && score[metric] <= 1, `${metric} out of range`);
    }
    const repeated = taggedFactMetrics({ expected, text: `${text} ${text}` });
    assert.equal(repeated.precision, score.precision, 'duplicate output changed precision');
    assert.equal(repeated.recall, score.recall, 'duplicate output changed recall');
    const harmonic = score.precision + score.recall === 0
      ? 0
      : (2 * score.precision * score.recall) / (score.precision + score.recall);
    assert.equal(score.f1, harmonic, 'F1 is not the harmonic mean');
    states += 1;
  }
}

console.log(`MODEL CHECK PASS: ${states} scorer states satisfy boundedness, idempotence and F1 invariants.`);
