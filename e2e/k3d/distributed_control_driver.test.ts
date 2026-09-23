import assert from 'node:assert/strict';
import test from 'node:test';
import {
  auditLoadResults,
  hasCompletedProviderMarker,
  loadShape,
  disposeFixtureSessions,
  nativeCrashHistoryErrors,
  validateFleetEnvironmentIds,
} from './distributed_control_driver.ts';

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

test('fleet load uses one unique Environment concurrency lane per Session', () => {
  // Causes: F1 count matches concurrency; F2 ids are nonempty; F3 ids are
  // unique. Effect: only F1+F2+F3 enters Session creation. Shared ids are the
  // separate one-active WorkQueue safety topology and cannot masquerade as a
  // fleet-throughput test. Rules cover success and each independent rejection.
  for (const concurrency of Array.from({ length: 12 }, (_, index) => index + 1)) {
    const exact=Array.from({length:concurrency},(_,index)=>`e${index}`);
    assert.deepEqual(validateFleetEnvironmentIds(exact,concurrency),exact);
    assert.throws(()=>validateFleetEnvironmentIds(exact.slice(1),concurrency),/one Environment/);
    if (concurrency>1) {
      assert.throws(()=>validateFleetEnvironmentIds(exact.map((id,index)=>index===1 ? exact[0] : id),concurrency),/must be unique/);
    }
    assert.throws(()=>validateFleetEnvironmentIds(exact.map((id,index)=>index===0 ? '' : id),concurrency),/must be nonempty/);
  }
});

test('Provider completion requires the exact marker followed by end_turn idle', () => {
  // Causes: R1 exact/missing marker; R2 idle before/after marker; R3 idle reason
  // requires_action/end_turn. Effect: only exact marker + later end_turn accepts.
  // This keeps approval and budget pauses from becoming load-test successes.
  const end = { type: 'session.status_idle', stop_reason: { type: 'end_turn' } };
  const ask = { type: 'session.status_idle', stop_reason: { type: 'requires_action' } };
  assert.equal(hasCompletedProviderMarker([response('one'), end], 'one'), true, 'R1+R2+R3');
  assert.equal(hasCompletedProviderMarker([end, response('one')], 'one'), false, 'R2');
  assert.equal(hasCompletedProviderMarker([response('one'), ask], 'one'), false, 'R3');
  assert.equal(hasCompletedProviderMarker([response('other'), end], 'one'), false, 'R1');
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

  // Scan-complexity rule: C1 one Session history contains M planned markers;
  // E1 its event content is inspected once, producing O(H+M) audit work. A
  // getter counter kills the former per-marker plus uniqueness rescans.
  let contentReads = 0;
  const sharedHistory = [{
    type: 'agent.message',
    get content() {
      contentReads += 1;
      return [
        { type: 'text', text: 'ADR71-PROVIDER:one' },
        { type: 'text', text: 'ADR71-PROVIDER:two' },
      ];
    },
  }];
  assert.deepEqual(auditLoadResults([sharedHistory], [['one', 'two']], [1, 2], 5000).errors, []);
  assert.equal(contentReads, 1, 'one Provider count index is shared by marker and uniqueness checks');
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

test('Native crash history separates terminal model evidence from final settlement', () => {
  // N1 missing, N2 duplicate, N3 terminal-before-idle and N4 settled are the
  // pure oracle used by both phases of the live crash scenario. Preparation may
  // observe N3; final verification cannot. An idle before the marker is never a
  // completion witness for that receipt.
  const idle = { type: 'session.status_idle', stop_reason: { type: 'end_turn' } };
  assert.ok(nativeCrashHistoryErrors([], 'one', false).length, 'N1');
  assert.ok(
    nativeCrashHistoryErrors([response('one'), response('one')], 'one', false).length,
    'N2',
  );
  assert.deepEqual(nativeCrashHistoryErrors([response('one')], 'one', false), [], 'N3 prepare');
  assert.ok(nativeCrashHistoryErrors([response('one')], 'one', true).length, 'N3 final');
  assert.ok(nativeCrashHistoryErrors([idle, response('one')], 'one', true).length, 'N3 ordering');
  assert.deepEqual(nativeCrashHistoryErrors([response('one'), idle], 'one', true), [], 'N4');
});

test('load campaign bounds are checked before any Session admission', () => {
  // L6 causes: default/custom count, concurrency and rounds; empty, fractional,
  // nonfinite, zero, negative, concurrency>count or rounds>100. Effects: one
  // canonical shape or rejection before any API call. R1 defaults; R2 bounded
  // endpoints; R3 each invalid dimension independently. Live L6 keeps each
  // window's original latency/history oracle, stopping at its first failure.
  assert.deepEqual(loadShape({}),{count:48,concurrency:12,rounds:1});
  for (const rounds of ['1','5','100']) assert.deepEqual(loadShape({
    ADR71_LOAD_REQUESTS:'1',ADR71_LOAD_CONCURRENCY:'1',ADR71_LOAD_ROUNDS:rounds,
  }),{count:1,concurrency:1,rounds:Number(rounds)});
  for (const field of ['ADR71_LOAD_REQUESTS','ADR71_LOAD_CONCURRENCY','ADR71_LOAD_ROUNDS']) {
    for (const value of ['','0','-1','0.5','NaN','Infinity']) {
      assert.throws(()=>loadShape({[field]:value}),/invalid load shape/);
    }
  }
  assert.throws(()=>loadShape({ADR71_LOAD_ROUNDS:'101'}),/invalid load shape/);
  assert.throws(()=>loadShape({ADR71_LOAD_CONCURRENCY:'49'}),/invalid load shape/);
});
