// e2e for the OTLP metrics path (#2): with OTLP configured, a real agent turn
// records the structure-only GenAI metrics (via the injected OtelMetricsRecorder)
// and the telemetry pipeline exports to a collector, and — critically — the server
// still shuts down CLEANLY (the metric provider's flush runs off the runtime thread
// with a bounded wait, so a pending export never hangs process exit).
//
// The inference-metric RECORDING is asserted directly by the Rust tests
// (`crates/runtime/awaken-runtime/tests/metrics.rs`); this e2e guards the export +
// shutdown wiring around it against a real OTLP/HTTP collector.
//
// Run: (from e2e/)  node metrics_export_e2e.mjs

import http from 'node:http';
import assert from 'node:assert/strict';
import { DefaultChatTransport } from 'ai';
import { Chat } from '@ai-sdk/react';
import { spawnServer, stopServer, waitForPort } from './harness.mjs';

const PORT = 38300;

function replyText(chat) {
  return (chat.lastMessage?.parts ?? [])
    .filter((p) => p.type === 'text')
    .map((p) => p.text)
    .join('');
}

async function main() {
  // A fake OTLP/HTTP collector: accept any POST, record the paths and whether the
  // structure-only inference metric name appears in the exported payload.
  const exports = [];
  let sawMetric = false;
  const receiver = http.createServer((req, res) => {
    const chunks = [];
    req.on('data', (c) => chunks.push(c));
    req.on('end', () => {
      const body = Buffer.concat(chunks);
      exports.push({ url: req.url, len: body.length });
      if (body.toString('latin1').includes('gen_ai.client.operation.count')) sawMetric = true;
      res.writeHead(200, { 'content-type': 'application/x-protobuf' });
      res.end();
    });
  });
  await new Promise((r) => receiver.listen(0, '127.0.0.1', r));
  const rport = receiver.address().port;

  const server = spawnServer('echo', PORT, {
    OTEL_EXPORTER_OTLP_ENDPOINT: `http://127.0.0.1:${rport}`,
    OTEL_EXPORTER_OTLP_PROTOCOL: 'http/protobuf',
    OTEL_METRIC_EXPORT_INTERVAL: '500',
  });

  let ranTurn = false;
  try {
    await waitForPort(PORT);
    const base = `http://127.0.0.1:${PORT}`;
    // A real turn: the engine records the GenAI inference metric on this path.
    const chat = new Chat({
      id: 'metrics-turn',
      transport: new DefaultChatTransport({ api: `${base}/v1/ai-sdk/threads/metrics-turn/runs` }),
    });
    await chat.sendMessage({ text: 'measure this turn' });
    assert.ok(
      replyText(chat).includes('measure this turn'),
      `turn did not complete: ${replyText(chat)}`,
    );
    ranTurn = true;
    console.log('  ok: ran a real turn with OTLP telemetry configured');
    // Let the periodic pipeline export before we tear down.
    await new Promise((r) => setTimeout(r, 1500));
  } finally {
    // If this hangs, the metric-flush shutdown regressed — the e2e would time out.
    await stopServer(server);
    receiver.close();
  }

  assert.ok(ranTurn, 'the run completed with metrics recording active');
  assert.ok(
    exports.length > 0,
    'the OTLP collector received telemetry from the metrics-enabled server',
  );
  assert.ok(
    sawMetric,
    `the structure-only inference metric was exported over OTLP; got paths ${JSON.stringify(exports.map((e) => e.url))}`,
  );
  // Reaching here means the server also shut down cleanly — the bounded metric
  // flush did not hang process exit.
  console.log(`  ok: the inference metric was exported over OTLP (${exports.length} export(s))`);
  console.log('E2E PASS: OTLP metrics exported and the server shut down cleanly (#2).');
}

main().catch((err) => {
  console.error('E2E FAIL:', err);
  process.exit(1);
});
