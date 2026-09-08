import assert from 'node:assert/strict';
import test from 'node:test';
import { auditLoadResults, disposeFixtureSessions } from './distributed_control_driver.ts';

const response = (marker: string) => ({ type: 'agent.message', content: [{ type: 'text', text: `ADR71-PROVIDER:${marker}` }] });

test('load audit crosses latency and complete-history failures independently', () => {
  // L5 decision table: C1 latency below/equal/above limit; C2 exact/missing/
  // duplicate terminal marker. E1 only valid latency + exact history accepts;
  // E2 both defects remain observable when C1 and C2 fail together. Cross all
  // nine cells: the old assert-before-history ordering loses those double faults.
  for (const elapsed of [4999, 5000, 5001]) {
    for (const copies of [0, 1, 2]) {
      const history = Array.from({ length: copies }, () => response('one'));
      const audit = auditLoadResults([history], [['one']], [elapsed], 5000);
      assert.equal(audit.errors.length === 0, elapsed <= 5000 && copies === 1);
      assert.equal(audit.errors.some((error) => error.includes('p95')), elapsed > 5000);
      assert.equal(audit.errors.some((error) => error.includes('Provider response')), copies !== 1);
    }
  }
});

test('load audit rejects absent samples, invalid measurements and hidden history', () => {
  // L6 C1 sample count differs from planned receipts; C2 time/limit is invalid;
  // C3 a different marker duplicates; C4 no workload was planned. Each rejects
  // independently. Preserve caller-owned samples; diagnostics must not mutate
  // the observations whose identities they are supposed to audit.
  const markers = [['one'], ['two']];
  const histories = [[response('one')], [response('two')]];
  const times = [200, 100];
  assert.deepEqual(auditLoadResults(histories, markers, times, 5000).errors, []);
  assert.deepEqual(times, [200, 100]);
  for (const samples of [[], [100], [100, 100, 100], [NaN, 100], [Infinity, 100], [-1, 100]]) {
    assert.ok(auditLoadResults(histories, markers, samples, 5000).errors.length);
  }
  for (const limit of [NaN, Infinity, 0, -1]) {
    assert.ok(auditLoadResults(histories, markers, times, limit).errors.length);
  }
  assert.ok(auditLoadResults([], [], [], 5000).errors.length);
  assert.ok(auditLoadResults([histories[0]], markers, times, 5000).errors.length);
  assert.ok(auditLoadResults([[...histories[0], response('other'), response('other')], histories[1]], markers, times, 5000).errors.length);
});

test('temporary batch cleanup joins every independent terminal request', async () => {
  // Batch-cleanup decision table. C1 zero/one/many auxiliary Sessions; C2 every
  // public disposal accepts or at least one rejects. Effects: E1 every supplied
  // auxiliary id is attempted exactly once; E2 success only when all accepted;
  // E3 one failure cannot short-circuit the independent cleanup requests. Rules:
  // B1 C1=zero => E1+E2; B2 C1=many+C2=all => E1+E2; B3 C1=many+C2=reject =>
  // E1+E3 and reject. The still-needed original Session is absent from the
  // supplied auxiliary set and therefore cannot be disposed by this owner.
  const calls: string[] = [];
  await disposeFixtureSessions([], async (id) => { calls.push(id); });
  await disposeFixtureSessions(['aux-a', 'aux-b'], async (id) => { calls.push(id); });
  assert.deepEqual(calls, ['aux-a', 'aux-b'], 'B1/B2');

  const rejectedCalls: string[] = [];
  await assert.rejects(
    disposeFixtureSessions(['aux-c', 'aux-d'], async (id) => {
      rejectedCalls.push(id);
      if (id === 'aux-c') throw new Error('disposal unavailable');
    }),
    /every temporary batch Session accepted terminal disposal/,
    'B3',
  );
  assert.deepEqual(rejectedCalls, ['aux-c', 'aux-d'], 'B3/E3');
});
