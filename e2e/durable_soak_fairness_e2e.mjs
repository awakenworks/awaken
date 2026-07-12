// Durable worker SOAK + FAIRNESS end to end, over the real server binary.
//
// The scaling k3d test already proves a single burst is exactly-once. This test
// adds the two properties that only show up under SUSTAINED, MULTI-THREAD load:
//
//   (a) SOAK — a bursty workload submitted in waves over a soak window, then a
//       full drain, with the process staying healthy the whole time; and
//   (b) FAIRNESS — many threads share the ONE durable queue + worker pool, and
//       NO thread starves: every thread's runs all complete, not just the ones
//       submitted first / on the busiest thread.
//
// Model exactly on durable_pool_e2e.mjs / managed_daemon_e2e.mjs: a single durable
// server (AWAKEN_INGRESS=durable + AWAKEN_DISPATCH_DAEMON=1) draining a shared
// SQLite dispatch queue with the deterministic `echo` model. Every run flows
// through submit_background → the shared queue → the worker; the caller observes
// committed truth by polling, it never drives a run.
//
// Workload: SOAK_THREADS distinct threads, each getting SOAK_WAVES runs, submitted
// as SOAK_WAVES bursty waves (one run per thread per wave) spaced SOAK_WAVE_DELAY_MS
// apart. Defaults keep the run ~30-45s (CI-friendly); tune via env for a longer soak.
//
// After a full drain we assert:
//   - Exactly-once completeness: every submitted run committed exactly once — the
//     committed echoes are exactly the submitted tags, no loss, no duplicate.
//   - Fairness / no starvation: EVERY thread completed ALL of its runs
//     (per_thread_min == per_thread_max == SOAK_WAVES); no thread has zero while
//     others have many.
//   - Stability / no leak: the dispatch backlog drained to zero (no Pending /
//     Running / Parked / DeadLetter / Superseded rows left) and the process never
//     crashed across the soak.
//
// Run: node e2e/durable_soak_fairness_e2e.mjs
//      SOAK_THREADS=40 SOAK_WAVES=8 node e2e/durable_soak_fairness_e2e.mjs

import assert from 'node:assert/strict';
import { mkdtempSync } from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { spawnServer, stopServer, waitForPort, pass } from './harness.mjs';

const PORT = Number(process.env.E2E_PORT ?? 38797);
const BASE = `http://127.0.0.1:${PORT}`;

// Tunables — defaults sized for a ~30-45s CI-friendly soak (20 threads * 5 waves
// = 100 runs over ~20s of bursty submission, then drain).
const THREADS = Number(process.env.SOAK_THREADS ?? 20);
const WAVES = Number(process.env.SOAK_WAVES ?? 5); // runs per thread
const WAVE_DELAY_MS = Number(process.env.SOAK_WAVE_DELAY_MS ?? 4000);
const DRAIN_TIMEOUT_MS = Number(process.env.SOAK_DRAIN_TIMEOUT_MS ?? 90_000);

// Durable ingress (shared queue + standing dispatch daemon) over a fresh SQLite
// storage dir, with the deterministic echo model — mirrors durable_pool_e2e.mjs.
const ENV = {
  AWAKEN_INGRESS: 'durable',
  AWAKEN_DISPATCH_DAEMON: '1',
  AWAKEN_STORAGE_DIR: mkdtempSync(path.join(tmpdir(), 'awaken-durable-soak-')),
};

const threadId = (i) => `soak-thread-${i}`;
const tagOf = (i, w) => `t${i}-w${w}`; // unique per (thread, wave); echoed back verbatim
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

async function submitBackground(thread, text) {
  const res = await fetch(`${BASE}/v1/durable/threads/${thread}/submit_background`, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify({ text }),
  });
  if (res.status !== 200) throw new Error(`submit_background ${thread} ${res.status}: ${await res.text()}`);
  const body = await res.json();
  if (!body.run_id || body.queued !== true) throw new Error(`unexpected submit body: ${JSON.stringify(body)}`);
  return body.run_id;
}

async function messages(thread) {
  const res = await fetch(`${BASE}/v1/durable/threads/${thread}/messages`);
  if (res.status !== 200) throw new Error(`messages ${thread} ${res.status}: ${await res.text()}`);
  return (await res.json()).messages ?? [];
}

async function dispatches(thread) {
  const res = await fetch(`${BASE}/v1/durable/threads/${thread}/dispatches`);
  if (res.status !== 200) throw new Error(`dispatches ${thread} ${res.status}: ${await res.text()}`);
  return (await res.json()).dispatches ?? [];
}

// The committed echoes on a thread, as the set of original tags (echo prepends
// "Echo: "). Only assistant messages with text are completed runs.
async function completedTags(thread) {
  const msgs = await messages(thread);
  return msgs
    .filter((m) => m.role === 'Assistant' && (m.text ?? '').startsWith('Echo: '))
    .map((m) => m.text.slice('Echo: '.length));
}

async function main() {
  const expected = new Set();
  for (let i = 0; i < THREADS; i++) for (let w = 0; w < WAVES; w++) expected.add(tagOf(i, w));
  const totalRuns = THREADS * WAVES;

  const { server } = spawnServer('echo', PORT, ENV);
  let crashed = false;
  server.on('exit', (code, signal) => {
    // A clean SIGINT shutdown in the finally block is expected; anything else
    // during the soak is a crash.
    if (signal !== 'SIGINT' && code !== 0 && code !== null) crashed = true;
  });
  try {
    await waitForPort(PORT);
    pass(`durable soak server up: ${THREADS} threads x ${WAVES} waves = ${totalRuns} runs over one shared queue`);

    // 1. Sustained, bursty submission: WAVES bursts, each a parallel fan-out of one
    //    run per thread, spaced WAVE_DELAY_MS apart so load is sustained, not a
    //    single spike. Everything flows through submit_background → queue → worker.
    const submitStart = Date.now();
    for (let w = 0; w < WAVES; w++) {
      const wave = [];
      for (let i = 0; i < THREADS; i++) wave.push(submitBackground(threadId(i), tagOf(i, w)));
      const results = await Promise.allSettled(wave);
      const failed = results.filter((r) => r.status === 'rejected');
      if (failed.length > 0) {
        throw new Error(`wave ${w}: ${failed.length}/${THREADS} submits failed, e.g. ${failed[0].reason}`);
      }
      assert.ok(!crashed, `server stayed healthy through submission wave ${w}`);
      if (w < WAVES - 1) await sleep(WAVE_DELAY_MS);
    }
    pass(`submitted ${totalRuns} runs across ${WAVES} bursty waves (${((Date.now() - submitStart) / 1000).toFixed(1)}s of sustained load)`);

    // 2. Wait for a FULL drain: poll committed truth until every thread has all of
    //    its runs echoed, or the deadline. Poll gently (the worker, not the poller,
    //    does the work).
    const deadline = Date.now() + DRAIN_TIMEOUT_MS;
    let perThread = new Map();
    for (;;) {
      perThread = new Map();
      let done = 0;
      for (let i = 0; i < THREADS; i++) {
        const tags = await completedTags(threadId(i));
        perThread.set(i, tags);
        if (tags.length >= WAVES) done++;
      }
      assert.ok(!crashed, 'server stayed healthy while draining');
      if (done === THREADS) break;
      if (Date.now() > deadline) {
        const starved = [...perThread.entries()]
          .filter(([, t]) => t.length < WAVES)
          .map(([i, t]) => `${threadId(i)}=${t.length}/${WAVES}`);
        throw new Error(
          `drain timed out after ${DRAIN_TIMEOUT_MS}ms: ${done}/${THREADS} threads fully drained; ` +
            `laggards: ${starved.slice(0, 10).join(', ')}${starved.length > 10 ? ', …' : ''}`,
        );
      }
      await sleep(500);
    }
    pass('every thread fully drained — the shared worker made progress on all threads');

    // 3a. Fairness / no starvation: per-thread completion counts.
    const counts = [...perThread.entries()].map(([i, t]) => ({ i, n: t.length }));
    const perThreadMin = Math.min(...counts.map((c) => c.n));
    const perThreadMax = Math.max(...counts.map((c) => c.n));
    const starved = counts.filter((c) => c.n === 0);
    assert.equal(starved.length, 0, `no thread starved (0 completed); starved: ${starved.map((c) => threadId(c.i)).join(', ')}`);
    assert.equal(perThreadMin, WAVES, `every thread completed ALL ${WAVES} of its runs (min=${perThreadMin})`);
    assert.equal(perThreadMax, WAVES, `no thread over-committed (max=${perThreadMax}, expected ${WAVES}) — no duplicate runs`);
    pass(`fairness held: per_thread_min=${perThreadMin} per_thread_max=${perThreadMax} (no starvation, no over-run)`);

    // 3b. Exactly-once completeness: the committed echoes are EXACTLY the submitted
    //     tags — no loss, no duplicate — across the whole soak.
    const seen = [];
    for (const [, tags] of perThread) seen.push(...tags);
    const completed = seen.length;
    const seenSet = new Set(seen);
    assert.equal(seen.length, seenSet.size, `no run committed twice (${seen.length} echoes, ${seenSet.size} distinct)`);
    assert.equal(completed, totalRuns, `exactly ${totalRuns} runs committed (got ${completed}) — no loss`);
    const missing = [...expected].filter((t) => !seenSet.has(t));
    const extra = [...seenSet].filter((t) => !expected.has(t));
    assert.equal(missing.length, 0, `no submitted run was lost (missing: ${missing.slice(0, 10).join(', ')})`);
    assert.equal(extra.length, 0, `no unexpected run committed (extra: ${extra.slice(0, 10).join(', ')})`);
    pass(`exactly-once held: ${completed}/${totalRuns} runs committed exactly once (no loss, no duplicate)`);

    // 3c. Stability / no leak: the dispatch backlog drained to zero — no rows left
    //     in any non-terminal or dead state on any thread.
    let backlog = 0;
    const stuck = [];
    for (let i = 0; i < THREADS; i++) {
      const rows = await dispatches(threadId(i));
      backlog += rows.length;
      if (rows.length > 0) stuck.push(`${threadId(i)}: ${rows.map((r) => r.status).join(',')}`);
    }
    assert.equal(backlog, 0, `dispatch backlog drained to zero (leftover: ${stuck.slice(0, 10).join(' | ')})`);
    assert.ok(!crashed, 'the server process never crashed across the soak');
    pass(`stability held: backlog drained to zero, process healthy across the soak`);

    console.log(
      `\nSUMMARY: threads=${THREADS} runs=${totalRuns} completed=${completed} ` +
        `per_thread_min=${perThreadMin} per_thread_max=${perThreadMax} backlog=0`,
    );
    pass('durable soak + fairness');
    console.log('\nDURABLE SOAK + FAIRNESS E2E PASS: sustained multi-thread durable load drained exactly-once and fairly, backlog zero, process healthy.');
  } finally {
    await stopServer(server);
  }
}

main().catch((err) => {
  console.error(`\nDURABLE SOAK + FAIRNESS E2E FAIL: ${err.stack ?? err}`);
  process.exit(1);
});
