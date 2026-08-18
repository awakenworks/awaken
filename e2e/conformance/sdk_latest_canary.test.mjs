import assert from 'node:assert/strict';
import test from 'node:test';
import { latestCanaryPlan } from './sdk_latest_canary_lib.mjs';

test('the current registry version reuses the reviewed installed declarations', () => {
  assert.deepEqual(latestCanaryPlan('0.117.1', '0.117.1'), {
    pinned: '0.117.1', latest: '0.117.1', fetchLatest: false,
  });
});

test('a newer registry version must be fetched for declaration differential', () => {
  assert.equal(latestCanaryPlan('0.117.1', '0.118.0').fetchLatest, true);
});

test('ranges and malformed registry responses fail closed', () => {
  assert.throws(() => latestCanaryPlan('^0.117.1', '0.117.1'), /exactly pinned/);
  assert.throws(() => latestCanaryPlan('0.117.1', 'latest'), /invalid/);
});
