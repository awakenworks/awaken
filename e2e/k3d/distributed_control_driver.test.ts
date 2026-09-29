import assert from 'node:assert/strict';
import test from 'node:test';
import {
  auditLoadResults,
  hasCompletedProviderMarker,
  loadShape,
  arrivalShape,
  runArrivalWindow,
  createParallelSessions,
  disposeFixtureSessions,
  nativeCrashHistoryErrors,
  nativeProviderInvocationErrors,
  nativeInFlightHistoryErrors,
  validateFleetEnvironmentIds,
} from './distributed_control_driver.ts';

test('open arrivals continue while all execution futures remain blocked', async () => {
  // O1 scheduled arrivals are independent of completion; O2 four held effects
  // remain below the generator budget; O3 all effects later return. O1+O2 must
  // launch at 0/500/1000/1500ms; O3 joins every effect and reports drain time.
  // A closed-loop mutant waiting for send before the next arrival fails here.
  let now = 0;
  const starts: number[] = [];
  const releases: (() => void)[] = [];
  const shape = arrivalShape({ ADR71_LOAD_RATE_PER_SECOND: '2' })!;
  const running = runArrivalWindow(4, shape, async (_, scheduled) => {
    starts.push(scheduled);
    await new Promise<void>((resolve) => releases.push(resolve));
  }, { now: () => now, sleep: async (ms) => { now += ms; } });
  for (let tick = 0; tick < 20; tick += 1) await Promise.resolve();
  assert.deepEqual(starts, [0, 500, 1000, 1500]);
  now = 2500;
  releases.forEach((release) => release());
  const observed = await running;
  assert.equal(observed.launched, 4);
  assert.equal(observed.drainMs, 500);
  assert.deepEqual(observed.errors, []);
});

test('arrival decision table rejects generator saturation and keeps effect errors', async () => {
  // O4 outstanding budget reached OR start-lag budget exceeded => explicit
  // unoffered request; never silently taper pressure. O5 synchronous/asynchronous
  // send failure => joined diagnostic; later schedules continue and cannot hide
  // the failure. Cross success/failure with latency and bounded outstanding.
  for (const failure of ['none', 'sync', 'async']) {
    let now = 0;
    const result = await runArrivalWindow(3, arrivalShape({ ADR71_LOAD_RATE_PER_SECOND: '1' })!, () => {
      if (failure === 'sync') throw new Error('sync effect');
      return failure === 'async' ? Promise.reject(new Error('async effect')) : Promise.resolve();
    }, { now: () => now, sleep: async (ms) => { now += ms; } });
    assert.equal(result.launched, 3);
    assert.equal(result.errors.length, failure === 'none' ? 0 : 3);
  }
  for (const reason of ['lag', 'outstanding']) {
    let now = 0;
    let release = () => {};
    const running = runArrivalWindow(3, arrivalShape({ ADR71_LOAD_RATE_PER_SECOND: '1', ADR71_LOAD_MAX_OUTSTANDING: '1' })!,
      () => new Promise<void>((resolve) => { release = resolve; }),
      { now: () => now, sleep: async (ms) => { now += ms + (reason === 'lag' ? 1001 : 0); } });
    for (let tick = 0; tick < 20; tick += 1) await Promise.resolve();
    release();
    const result = await running;
    assert.equal(result.launched, reason === 'lag' ? 0 : 1);
    assert.equal(result.errors.length, 1);
  }
});

test('arrival configuration rejects each invalid axis before admission', () => {
  // O6 absent rate retains the existing closed regression mode. O7 explicit
  // rate/budget/lag/p99 must all be positive finite bounded values. Negate each
  // axis independently; accepted boundaries prevent reject-all false greens.
  assert.equal(arrivalShape({}), null);
  assert.equal(arrivalShape({ ADR71_LOAD_RATE_PER_SECOND: '10000' })!.rate, 10000);
  for (const field of ['ADR71_LOAD_RATE_PER_SECOND', 'ADR71_LOAD_MAX_OUTSTANDING',
    'ADR71_LOAD_MAX_START_LAG_MS', 'ADR71_TURN_P99_MS']) {
    for (const value of ['', '0', '-1', 'NaN', 'Infinity']) {
      assert.throws(() => arrivalShape({ ADR71_LOAD_RATE_PER_SECOND: '1', [field]: value }), /invalid arrival/);
    }
  }
  assert.throws(() => arrivalShape({ ADR71_LOAD_RATE_PER_SECOND: '10001' }), /invalid arrival/);
  assert.throws(() => arrivalShape({ ADR71_LOAD_RATE_PER_SECOND: '1', ADR71_LOAD_MAX_OUTSTANDING: '1.5' }), /invalid arrival/);
});

const response = (marker: string) => ({ type: 'agent.message', content: [{ type: 'text', text: `ADR71-PROVIDER:${marker}` }] });

test('partial Session admission joins all effects before canonical disposal', async () => {
  // A1 one create fails/replies without id while its peers succeed; A2 one
  // cleanup may fail. A1=>dispose both exact peer ids, never replay create;
  // A1+A2=>retain admission AND cleanup errors. A3 all creates succeed=>no
  // early deletion. Independent complete and partial rules cover both effects.
  for (const failure of ['none', 'reject', 'missing-id']) {
    for (const cleanupFailure of [false, true]) {
      const deleted: string[] = [];
      let creates = 0;
      const client = { beta: { sessions: {
        create: async ({ environment_id }: {environment_id: string}) => {
          creates += 1;
          if (environment_id === 'b' && failure === 'reject') throw new Error('lost response');
          return environment_id === 'b' && failure === 'missing-id' ? {} : {id: environment_id};
        },
        delete: async (id: string) => { deleted.push(id); if (cleanupFailure) throw new Error('cleanup unavailable'); },
      } } };
      if (failure === 'none') {
        assert.deepEqual(await createParallelSessions(client as any, ['a','b','c']), ['a','b','c']);
        assert.deepEqual(deleted, []);
      } else {
        await assert.rejects(createParallelSessions(client as any, ['a','b','c']),
          cleanupFailure ? /Session admission failed:.*partial admission cleanup/ : /Session admission failed/);
        assert.deepEqual(deleted.sort(), ['a','c']);
      }
      assert.equal(creates, 3);
    }
  }
});

test('independent Provider double reports exact started work and retained completion', () => {
  // I1 running + one accepted/started => admit pre-crash injection; I2 one
  // complete exact result => admit post-crash observation. Each identity,
  // cardinality and phase axis is independently negated, with positive rows
  // preventing a reject-everything oracle. This is fixture evidence only.
  const marker='one';
  for (const complete of [false,true]) {
    const exact={marker,accepted:1,started:1,running:Number(!complete),completed:Number(complete),results:complete?['ADR71-PROVIDER:one']:[]};
    assert.deepEqual(nativeProviderInvocationErrors(exact,marker,complete),[]);
    for (const changed of [
      {...exact,marker:'other'}, {...exact,accepted:0}, {...exact,accepted:2},
      {...exact,started:0}, {...exact,started:2}, {...exact,running:2},
      {...exact,completed:2}, {...exact,results:['wrong']},
    ]) assert.ok(nativeProviderInvocationErrors(changed,marker,complete).length);
  }
  assert.ok(nativeProviderInvocationErrors(null,marker,false).length);
});

test('unknown in-flight history keeps logical errors separate from physical quiescence', () => {
  // U1 original accepted receipt, no model result/success => accepted even if
  // processed_at or an error outcome exists. U2 missing receipt, duplicate id,
  // any exact model result, or end_turn AFTER receipt => rejects. Initial idle
  // BEFORE the receipt cannot become a false completion for this attempt.
  const receipt={id:'receipt',type:'user.message',processed_at:'observed'};
  const end={id:'end',type:'session.status_idle',stop_reason:{type:'end_turn'}};
  const error={...end,stop_reason:{type:'error'}};
  for(const history of [[receipt],[end,receipt],[receipt,error]]) {
    assert.deepEqual(nativeInFlightHistoryErrors(history,'receipt','one'),[]);
  }
  for(const history of [[],[receipt,receipt],[receipt,{id:'model',...response('one')}],[receipt,end]]) {
    assert.ok(nativeInFlightHistoryErrors(history,'receipt','one').length);
  }
});

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

test('load audit scaling matrix retains linear history ownership', () => {
  // Scaling matrix: H=M=100/1k/10k exact Provider results in one Session.
  // Effects: one pass indexes history, every planned marker is accepted once,
  // and timing is diagnostic only; the public K3D p95 remains the SLO owner.
  for (const historySize of [100, 1_000, 10_000]) {
    const markers = Array.from({ length: historySize }, (_, index) => `m-${index}`);
    const history = markers.map(response);
    const latencies = markers.map((_, index) => index + 1);
    const started = performance.now();
    const audit = auditLoadResults([history], [markers], latencies, historySize + 1);
    const elapsed = performance.now() - started;
    assert.deepEqual(audit.errors, [], `H=M=${historySize}`);
    process.stderr.write(`LOAD_AUDIT_BENCH ${JSON.stringify({ historySize, elapsedMs: elapsed })}\n`);
  }
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
