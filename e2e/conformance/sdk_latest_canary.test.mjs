import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';
import { latestCanaryPlan } from './sdk_latest_canary_lib.mjs';

test('registry latest reuses the generated current oracle only when all versions agree', () => {
  // Cause/effect graph: C1 generated current oracle is exact; C2 installed
  // current.module equals C1; C3 registry latest equals C1; C4 registry drift is
  // younger than pnpm's one release-age policy; C5 drift is mature; C6 any
  // version or policy evidence is invalid. Effects: E1 run the installed oracle;
  // E2 quarantine without installing the candidate; E3 require regeneration;
  // E4 reject stale node_modules; E5 reject ambiguous evidence. Decision table:
  // R1 C1+C2+C3=>E1; R2 C1+C2+!C3+C4=>E1+E2;
  // R3 C1+C2+!C3+C5=>E3; R4 C1+!C2=>E4; R5 C6=>E5. Constraint:
  // the canary never downloads or fingerprints a second SDK outside the oracle.
  assert.deepEqual(latestCanaryPlan('0.121.0', '0.121.0', '0.121.0'), {
    oracle: '0.121.0', latest: '0.121.0', installed: '0.121.0',
  });
});

test('a newer registry version remains quarantined during the dependency observation period', () => {
  // R2: young drift is visible but cannot become executable dependency evidence.
  assert.deepEqual(
    latestCanaryPlan('0.120.0', '0.121.0', '0.120.0', {
      latestPublishedAt: '2026-08-27T20:35:25.000Z',
      minimumReleaseAgeMinutes: 1_440,
      now: Date.parse('2026-08-27T22:35:25.000Z'),
    }),
    {
      oracle: '0.120.0',
      latest: '0.121.0',
      installed: '0.120.0',
      quarantinedUntil: '2026-08-28T20:35:25.000Z',
    },
  );
});

test('a mature newer registry version fails closed until the current oracle is regenerated', () => {
  // R3: elapsed observation time turns drift into required oracle maintenance.
  assert.throws(
    () => latestCanaryPlan('0.120.0', '0.121.0', '0.120.0', {
      latestPublishedAt: '2026-08-27T20:35:25.000Z',
      minimumReleaseAgeMinutes: 1_440,
      now: Date.parse('2026-08-28T20:35:25.000Z'),
    }),
    /update the current anchor and regenerate/u,
  );
});

test('a stale installed SDK fails before it can impersonate the pin', () => {
  // R4: installed evidence must match before registry policy is considered.
  assert.throws(
    () => latestCanaryPlan('0.121.0', '0.121.0', '0.120.0'),
    /does not match oracle/u,
  );
});

test('ranges and malformed registry responses fail closed', () => {
  // R5: ambiguous version evidence is rejected before comparison.
  assert.throws(() => latestCanaryPlan('^0.121.0', '0.121.0', '0.121.0'), /must be exact/u);
  assert.throws(() => latestCanaryPlan('0.121.0', 'latest', '0.121.0'), /invalid/u);
  assert.throws(() => latestCanaryPlan('0.121.0', ['0.121.0'], '0.121.0'), /invalid/u);
});

test('registry drift without one valid release-age policy fails closed', () => {
  // R5: drift cannot invent a grace period when any policy coordinate is absent.
  assert.throws(
    () => latestCanaryPlan('0.120.0', '0.121.0', '0.120.0'),
    /valid minimum-release-age policy/u,
  );
  assert.throws(
    () => latestCanaryPlan('0.120.0', '0.121.0', '0.120.0', {
      latestPublishedAt: 'invalid', minimumReleaseAgeMinutes: 1_440, now: Date.now(),
    }),
    /valid minimum-release-age policy/u,
  );
});

test('the release canary reaches both official TypeScript and Python SDK oracles', () => {
  // Orchestration cause/effect graph: C1=the release entry executes the local
  // multi-language oracle check; C2=it executes the online Python wheel canary;
  // C3=it executes the TypeScript runtime canary. Effect E1=no language can be
  // silently dropped while the outer `test:sdk-latest-canary` command remains
  // green. Decision table: C1+C2+C3=>E1; removal of any exact edge=>reject.
  const source = readFileSync(new URL('./sdk_latest_canary.mjs', import.meta.url), 'utf8');
  for (const edge of [
    "'check'",
    "'check:python:online'",
    "'sdk_latest_runtime_canary.mjs'",
  ]) {
    assert.ok(source.includes(edge), `release canary is missing ${edge}`);
  }
  const scripts = JSON.parse(readFileSync(new URL('../package.json', import.meta.url), 'utf8')).scripts;
  assert.match(scripts['test:sdk-latest-canary'], /npm run test:sdk-python-runtime/u);
  assert.equal(
    scripts['test:sdk-python-runtime'],
    'node conformance/managed_python_sdk_runtime_e2e.mjs',
  );
});
