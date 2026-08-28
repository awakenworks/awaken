import assert from 'node:assert/strict';
import test from 'node:test';
import { assertPendingReceiptHasNoRuntimeEffects } from '../harness.mjs';

const prior = { id: 'event-prior', type: 'agent.message', processed_at: 'committed' };
const pending = { id: 'event-pending', type: 'user.message', processed_at: null };

function qualify(history, priorHistory = []) {
  return assertPendingReceiptHasNoRuntimeEffects({
    history,
    priorHistory,
    receiptId: pending.id,
    forbiddenEventTypes: new Set(['agent.message', 'session.error']),
    description: 'pending receipt oracle',
  });
}

test('pending receipt oracle accepts one exact unprocessed suffix without new effects', () => {
  // Cause/effect graph: C1=older committed history exists; C2=one root receipt
  // is durably accepted; C3=no Runtime anchor/effect commits. Effects: E1=the
  // receipt is listable exactly once with null processed_at; E2=older effects
  // are not mistaken for new effects. Decision rule C1+C2+C3=>E1+E2.
  assert.deepEqual(qualify([prior, pending], [prior]), [pending]);
});

test('pending receipt oracle fails closed for every violated ownership coordinate', () => {
  // Boundary decision table:
  // | exact count | processed | new forbidden effect | result |
  // | 1           | null      | no                   | accept |
  // | 0 or 2      | null      | no                   | reject |
  // | 1           | timestamp | no                   | reject |
  // | 1           | null      | yes                  | reject |
  // | 1 in prior  | null      | no                   | reject |
  assert.throws(() => qualify([]), /one pending receipt/u, 'missing');
  assert.throws(() => qualify([pending, pending]), /one pending receipt/u, 'duplicate');
  assert.throws(
    () => qualify([{ ...pending, processed_at: 'committed' }]),
    /marked.*processed/u,
    'false processing',
  );
  assert.throws(
    () => qualify([pending, { id: 'effect', type: 'session.error', processed_at: 'committed' }]),
    /fabricated execution or terminal effects/u,
    'new forbidden effect',
  );
  assert.throws(() => qualify([pending], [pending]), /did not add/u, 'not a new receipt');
});
