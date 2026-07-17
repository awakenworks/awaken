// Durable worker/dispatch OPERATIONAL METRICS — what the durable path actually
// exports, scraped from the real Prometheus `/metrics` endpoint.
//
// FINDING (observability gap): the durable dispatch/worker layer emits NO
// operational counters. There is no runs-claimed / runs-settled / runs-parked
// counter, no queue-depth gauge, and no drive-duration histogram anywhere in the
// dispatch pool or run-ingress crates. The ONLY metrics a server exports are:
//
//   1. Prometheus text at `GET /metrics` (mounted by `with_brain_admin`,
//      crates/bin/awaken-server/src/brain_admin.rs) — two CONNECTION/lifecycle
//      gauges, not dispatch counters:
//        - `awaken_brain_active_streams`  (gauge) in-flight requests; a foreground
//          durable run holds its HTTP handler open while the dispatch worker drives
//          it, so this gauge reflects durable in-flight work (KEDA autoscales on it).
//        - `awaken_brain_draining`        (gauge) 1 while draining for scale-in.
//   2. OTLP-only GenAI/tool instruments via `OtelMetricsRecorder`
//      (crates/runtime/awaken-observability/src/metrics.rs): gen_ai.client.* and
//      awaken.tool.execution.* — model/tool metrics, NOT durable-path metrics, and
//      exported over OTLP only (never on `/metrics`). Covered by metrics_export_e2e.
//
// So this test does NOT fabricate dispatch counters. It boots a DURABLE server
// (AWAKEN_INGRESS=durable), drives real runs through the enqueue→claim→worker→commit
// path, and asserts the metrics that GENUINELY exist MOVE as durable work flows:
//   - the two gauges are exported and well-formed at baseline (both 0),
//   - `awaken_brain_active_streams` rises above 0 while durable runs are in flight
//     (the exported operational signal for durable worker load), then settles to 0,
//   - `awaken_brain_draining` flips 0→1 on POST /admin/drain and /readyz goes 503.
//
// Asserted metric NAMES are taken verbatim from the Rust source; none invented.
//
// Run: (from e2e/)  node durable_worker_metrics_e2e.mjs

import fs from 'node:fs';
import assert from 'node:assert/strict';
import { DefaultChatTransport } from 'ai';
import { Chat } from '@ai-sdk/react';
import { spawnServer, stopServer, waitForPort, pass } from './harness.mjs';

const PORT = Number(process.env.E2E_PORT ?? 38310);
const STORE = `/tmp/awaken-durable-worker-metrics-${process.pid}`;
const BASE = `http://127.0.0.1:${PORT}`;
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

function replyText(chat) {
  return (chat.lastMessage?.parts ?? [])
    .filter((p) => p.type === 'text')
    .map((p) => p.text)
    .join('');
}

// Scrape the Prometheus `/metrics` endpoint and return the raw text.
async function scrape() {
  const res = await fetch(`${BASE}/metrics`);
  assert.equal(res.status, 200, `GET /metrics returned ${res.status}`);
  return res.text();
}

// Parse a single sample out of Prometheus text. The OTel exporter appends scope
// labels (`name{otel_scope_name="awaken-brain"} <value>`), so match the metric name
// with optional `{...}` and read the value after the final space.
function gauge(text, name) {
  const line = text
    .split('\n')
    .find((l) => l.startsWith(`${name} `) || l.startsWith(`${name}{`));
  assert.ok(line, `metric ${name} not present in /metrics:\n${text}`);
  const v = Number(line.slice(line.lastIndexOf(' ') + 1).trim());
  assert.ok(Number.isFinite(v), `metric ${name} not a number: ${line}`);
  return { line, v };
}

const dispatchDbs = (dir) =>
  fs
    .readdirSync(dir, { withFileTypes: true })
    .filter((e) => e.isFile() && e.name.endsWith('dispatch.db'))
    .map((e) => e.name)
    .sort();

async function main() {
  fs.rmSync(STORE, { recursive: true, force: true });
  fs.mkdirSync(STORE, { recursive: true });

  // A DURABLE server: AWAKEN_INGRESS=durable makes the managed host deliver every
  // turn through the persistent dispatch queue driven by the process dispatch pool
  // (enqueue → claim → lease → worker execute → commit). echo is a deterministic
  // in-process stub, so no upstream/API key needed and the run is CI-safe.
  const { server } = spawnServer('echo', PORT, {
    AWAKEN_INGRESS: 'durable',
    AWAKEN_STORAGE_DIR: STORE,
  });

  try {
    await waitForPort(PORT);

    // ---- 1. baseline: the exported gauges are present, well-formed, and zero ----
    const base = await scrape();
    const a0 = gauge(base, 'awaken_brain_active_streams');
    const d0 = gauge(base, 'awaken_brain_draining');
    assert.equal(a0.v, 0, 'active_streams starts at 0 (no real traffic in flight)');
    assert.equal(d0.v, 0, 'draining starts at 0');
    // The scrape itself is on an admin route excluded from the in-flight counter, so
    // reading /metrics never inflates active_streams — it reflects only real runs.
    console.log(`  baseline:\n    ${a0.line}\n    ${d0.line}`);
    pass('durable server exports the /metrics gauges at baseline (both 0)');

    // ---- 2. drive durable runs and catch active_streams above baseline ----
    // Fire N concurrent runs through the durable dispatch worker without awaiting,
    // then tight-poll /metrics to observe the in-flight gauge rise. Each foreground
    // durable run holds its HTTP handler open until the worker settles it, so
    // concurrent runs light up `awaken_brain_active_streams`.
    const N = 12;
    const runs = [];
    for (let i = 0; i < N; i++) {
      const id = `dwm-${i}`;
      const chat = new Chat({
        id,
        transport: new DefaultChatTransport({ api: `${BASE}/v1/ai-sdk/threads/${id}/runs` }),
      });
      runs.push(
        chat.sendMessage({ text: `durable-metric-${i}` }).then(() => replyText(chat)),
      );
    }

    // Poll continuously while the runs are in flight, tracking the peak in-flight
    // count the exported gauge reports.
    let peak = 0;
    let peakLine = a0.line;
    let settled = false;
    Promise.allSettled(runs).then(() => {
      settled = true;
    });
    for (let i = 0; i < 400 && !settled; i++) {
      const s = gauge(await scrape(), 'awaken_brain_active_streams');
      if (s.v > peak) {
        peak = s.v;
        peakLine = s.line;
      }
    }
    const replies = await Promise.all(runs);
    for (let i = 0; i < N; i++) {
      assert.ok(
        replies[i].includes(`durable-metric-${i}`),
        `durable run ${i} did not drive through the worker and echo: ${replies[i]}`,
      );
    }
    pass(`${N} runs drove through the durable dispatch worker (enqueue→claim→worker→commit)`);

    // The run-ingress durable artifact on disk — proves the queue+worker path (not a
    // direct in-memory ingress) actually delivered these runs.
    const dbs = dispatchDbs(STORE);
    assert.ok(dbs.length >= 1, `durable dispatch queue on disk (got: ${dbs.join(', ') || 'none'})`);
    pass(`durable dispatch queue persisted on disk: ${dbs.join(', ')}`);

    // The exported operational signal MOVED: in-flight durable worker load was
    // visible on the gauge (> baseline of 0).
    assert.ok(
      peak >= 1,
      `awaken_brain_active_streams never rose above 0 while ${N} durable runs were in flight`,
    );
    console.log(`  peak in-flight during durable drive:\n    ${peakLine}`);
    pass(`awaken_brain_active_streams rose to ${peak} under durable worker load (exported gauge moved)`);

    // ---- 3. after the drive: the gauge settles back to 0 ----
    let settledText;
    for (let i = 0; i < 100; i++) {
      settledText = await scrape();
      if (gauge(settledText, 'awaken_brain_active_streams').v === 0) break;
      await sleep(20);
    }
    const aEnd = gauge(settledText, 'awaken_brain_active_streams');
    assert.equal(aEnd.v, 0, 'active_streams returned to 0 once every durable run settled');
    console.log(`  after drain of in-flight work:\n    ${aEnd.line}`);
    pass('awaken_brain_active_streams settled back to 0 after the durable runs completed');

    // ---- 4. lifecycle metric moves: draining flips on /admin/drain ----
    const drainRes = await fetch(`${BASE}/admin/drain`, { method: 'POST' });
    assert.equal(drainRes.status, 200, `POST /admin/drain returned ${drainRes.status}`);
    const drained = gauge(await scrape(), 'awaken_brain_draining');
    assert.equal(drained.v, 1, 'awaken_brain_draining flipped to 1 after /admin/drain');
    const ready = await fetch(`${BASE}/readyz`);
    assert.equal(ready.status, 503, 'readyz reports 503 once draining (scale-in signal)');
    console.log(`  after /admin/drain:\n    ${drained.line}`);
    pass('awaken_brain_draining flipped 0→1 on drain (and /readyz → 503) — lifecycle gauge moved');

    console.log(
      'E2E PASS: durable worker path exports the /metrics connection+lifecycle gauges ' +
        'and they MOVE with durable work (active_streams, draining).',
    );
    console.log(
      'GAP: no durable-dispatch operational counters exist — no runs-claimed/settled/parked ' +
        'counter, no queue-depth gauge, no drive-duration histogram. See report / recommend adding.',
    );
  } finally {
    await stopServer(server);
    fs.rmSync(STORE, { recursive: true, force: true });
  }
}

main().catch((err) => {
  console.error('E2E FAIL:', err);
  process.exit(1);
});
