import assert from 'node:assert/strict';
import test from 'node:test';
import { latestCanaryPlan } from './sdk_latest_canary_lib.mjs';

test('the current registry version reuses the reviewed installed declarations', () => {
  // Cause/effect graph: C1 pinned=latest=installed; C2 latest is newer than
  // pinned while installed still equals pinned; C3 installed differs from the
  // exact pin; C4 a version is ranged/malformed. Effects: E1 reuse reviewed
  // declarations; E2 fetch latest; E3 reject stale local declarations; E4
  // reject an ambiguous version. Decision rules: R1 C1=>E1; R2 C2=>E2;
  // R3 C3=>E3; R4 C4=>E4. K1 a registry comparison never upgrades the meaning
  // of stale node_modules into evidence for the pinned SDK.
  assert.deepEqual(latestCanaryPlan('0.117.1', '0.117.1', '0.117.1'), {
    pinned: '0.117.1', latest: '0.117.1', installed: '0.117.1', fetchLatest: false,
  });
});

test('a newer registry version must be fetched for declaration differential', () => {
  assert.equal(latestCanaryPlan('0.117.1', '0.118.0', '0.117.1').fetchLatest, true);
});

test('a stale installed SDK fails before it can impersonate the pin', () => {
  assert.throws(
    () => latestCanaryPlan('0.120.0', '0.120.0', '0.117.1'),
    /does not match pinned/,
  );
});

test('ranges and malformed registry responses fail closed', () => {
  assert.throws(() => latestCanaryPlan('^0.117.1', '0.117.1', '0.117.1'), /exactly pinned/);
  assert.throws(() => latestCanaryPlan('0.117.1', 'latest', '0.117.1'), /invalid/);
});
