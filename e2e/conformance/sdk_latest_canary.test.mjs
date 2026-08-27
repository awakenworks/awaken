import assert from 'node:assert/strict';
import test from 'node:test';
import { latestCanaryPlan } from './sdk_latest_canary_lib.mjs';

test('registry latest reuses the generated current oracle only when all versions agree', () => {
  // Cause/effect graph: C1 generated current oracle is exact; C2 registry latest
  // equals C1; C3 the module resolved by current.module has C1; C4 any version
  // is malformed. Effects: E1 reuse that one installed anchor; E2 registry drift
  // fails closed until anchor generation; E3 reject stale node_modules; E4
  // reject ambiguous evidence. Decision table: R1 C1+C2+C3=>E1;
  // R2 C1+!C2=>E2; R3 C1+C2+!C3=>E3; R4 C4=>E4. Constraint: the canary
  // cannot download or fingerprint a second SDK package outside the oracle.
  assert.deepEqual(latestCanaryPlan('0.121.0', '0.121.0', '0.121.0'), {
    oracle: '0.121.0', latest: '0.121.0', installed: '0.121.0',
  });
});

test('a newer registry version fails closed until the current oracle is regenerated', () => {
  assert.throws(
    () => latestCanaryPlan('0.120.0', '0.121.0', '0.120.0'),
    /update the current anchor and regenerate/u,
  );
});

test('a stale installed SDK fails before it can impersonate the pin', () => {
  assert.throws(
    () => latestCanaryPlan('0.121.0', '0.121.0', '0.120.0'),
    /does not match oracle/u,
  );
});

test('ranges and malformed registry responses fail closed', () => {
  assert.throws(() => latestCanaryPlan('^0.121.0', '0.121.0', '0.121.0'), /must be exact/u);
  assert.throws(() => latestCanaryPlan('0.121.0', 'latest', '0.121.0'), /invalid/u);
  assert.throws(() => latestCanaryPlan('0.121.0', ['0.121.0'], '0.121.0'), /invalid/u);
});
