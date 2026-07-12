// e2e for the durable DISPATCH operational metrics over OTLP.
//
// GAP CLOSED: the durable dispatch/worker layer previously emitted NO operational
// metrics (see the note in durable_worker_metrics_e2e.mjs — no runs-claimed /
// runs-settled counter, no drive-duration histogram). The worker now records, on
// the SAME injected recorder that meters model/tool calls (so it exports on the one
// OTLP pipeline with no extra wiring):
//   - `awaken.dispatch.runs.claimed`  (counter) per claimed dispatch driven,
//   - `awaken.dispatch.runs.settled`  (counter, `outcome`=done|parked) per settle,
//   - `awaken.dispatch.drive.duration` (histogram, seconds) per drive_claimed.
//
// This test boots a DURABLE server (AWAKEN_INGRESS=durable) pointed at a fake
// OTLP/HTTP collector, drives several real runs through the enqueue→claim→worker→
// commit path, and asserts the exported OTLP payload carries all three metric names
// (verbatim from the Rust source) — proving the dispatch instruments recorded data
// points and left the process over the shared OTLP pipeline. It also proves the
// server still shuts down cleanly (the bounded metric flush never hangs exit).
//
// Asserted metric NAMES are taken verbatim from the Rust source; none invented.
//
// Run: (from e2e/)  node dispatch_metrics_export_e2e.mjs

import http from 'node:http';
import fs from 'node:fs';
import assert from 'node:assert/strict';
import { DefaultChatTransport } from 'ai';
import { Chat } from '@ai-sdk/react';
import { spawnServer, stopServer, waitForPort, pass } from './harness.mjs';

const PORT = Number(process.env.E2E_PORT ?? 38320);
const STORE = `/tmp/awaken-dispatch-metrics-${process.pid}`;

function replyText(chat) {
  return (chat.lastMessage?.parts ?? [])
    .filter((p) => p.type === 'text')
    .map((p) => p.text)
    .join('');
}

async function main() {
  fs.rmSync(STORE, { recursive: true, force: true });
  fs.mkdirSync(STORE, { recursive: true });

  // A fake OTLP/HTTP collector: accept any POST and accumulate the raw payload so we
  // can look for the dispatch metric names the SDK serializes into the protobuf.
  const bodies = [];
  const receiver = http.createServer((req, res) => {
    const chunks = [];
    req.on('data', (c) => chunks.push(c));
    req.on('end', () => {
      bodies.push(Buffer.concat(chunks).toString('latin1'));
      res.writeHead(200, { 'content-type': 'application/x-protobuf' });
      res.end();
    });
  });
  await new Promise((r) => receiver.listen(0, '127.0.0.1', r));
  const rport = receiver.address().port;

  // A DURABLE server: every turn is delivered through the persistent dispatch queue
  // driven by the process dispatch pool (enqueue → claim → lease → worker → commit),
  // so the worker's `awaken.dispatch.*` instruments actually fire. echo is a
  // deterministic in-process stub — CI-safe, no upstream/API key needed.
  const { server } = spawnServer('echo', PORT, {
    AWAKEN_INGRESS: 'durable',
    AWAKEN_STORAGE_DIR: STORE,
    OTEL_EXPORTER_OTLP_ENDPOINT: `http://127.0.0.1:${rport}`,
    OTEL_EXPORTER_OTLP_PROTOCOL: 'http/protobuf',
    OTEL_METRIC_EXPORT_INTERVAL: '500',
  });

  const has = (needle) => bodies.some((b) => b.includes(needle));

  try {
    await waitForPort(PORT);
    const base = `http://127.0.0.1:${PORT}`;

    // Drive several durable runs. Each foreground durable run holds its HTTP handler
    // open until the worker claims, drives, and settles it — so each one increments
    // runs.claimed, records drive.duration, and (echo ends naturally) settles `done`.
    const N = 5;
    const runs = [];
    for (let i = 0; i < N; i++) {
      const id = `dm-${i}`;
      const chat = new Chat({
        id,
        transport: new DefaultChatTransport({ api: `${base}/v1/ai-sdk/threads/${id}/runs` }),
      });
      runs.push(chat.sendMessage({ text: `dispatch-metric-${i}` }).then(() => replyText(chat)));
    }
    const replies = await Promise.all(runs);
    for (let i = 0; i < N; i++) {
      assert.ok(
        replies[i].includes(`dispatch-metric-${i}`),
        `durable run ${i} did not drive through the worker and echo: ${replies[i]}`,
      );
    }
    pass(`${N} runs drove through the durable dispatch worker (enqueue→claim→worker→commit)`);

    // Let the periodic OTLP pipeline flush the recorded dispatch metrics.
    for (let i = 0; i < 40 && !(has('awaken.dispatch.runs.claimed') &&
      has('awaken.dispatch.runs.settled') && has('awaken.dispatch.drive.duration')); i++) {
      await new Promise((r) => setTimeout(r, 200));
    }
  } finally {
    // If this hangs, the metric-flush shutdown regressed — the e2e would time out.
    await stopServer(server);
    receiver.close();
    fs.rmSync(STORE, { recursive: true, force: true });
  }

  assert.ok(bodies.length > 0, 'the OTLP collector received telemetry from the durable server');

  // The three dispatch instruments each recorded ≥1 data point and were exported.
  assert.ok(
    has('awaken.dispatch.runs.claimed'),
    'awaken.dispatch.runs.claimed was exported over OTLP (worker counted the claims)',
  );
  assert.ok(
    has('awaken.dispatch.runs.settled'),
    'awaken.dispatch.runs.settled was exported over OTLP (worker counted the settles)',
  );
  assert.ok(
    has('awaken.dispatch.drive.duration'),
    'awaken.dispatch.drive.duration histogram was exported over OTLP (worker timed the drives)',
  );
  // The settle counter's outcome label is present; echo runs end naturally → `done`.
  assert.ok(has('done'), 'runs.settled carried an outcome=done data point');
  pass('all three awaken.dispatch.* metrics exported over the shared OTLP pipeline');

  // Reaching here means the durable server also shut down cleanly — the bounded
  // metric flush did not hang process exit.
  console.log(
    `E2E PASS: durable dispatch worker exports awaken.dispatch.runs.claimed/settled ` +
      `and drive.duration over OTLP, and the server shut down cleanly.`,
  );
}

main().catch((err) => {
  console.error('E2E FAIL:', err);
  process.exit(1);
});
