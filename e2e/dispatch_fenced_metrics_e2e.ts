// A live stale-attempt race through the durable API. Attempt A remains inside a
// real provider call while its lease is expired and recovered; attempt B commits
// the run, then A returns and its settlement is fenced. The process OTLP pipeline
// must export the dedicated fenced counter.

import assert from 'node:assert/strict';
import fs from 'node:fs';
import http from 'node:http';
import { execFileSync } from 'node:child_process';
import { Chat } from '@ai-sdk/react';
import { DefaultChatTransport } from 'ai';
import { realServerEnv, spawnServer, startUpstream, stopServer, waitForPort } from './harness.mjs';

const PORT = Number(process.env.E2E_PORT ?? 38321);
const STORE = `/tmp/awaken-dispatch-fenced-${process.pid}`;
const THREAD = `dispatch-fenced-${process.pid}`;

async function waitUntil(label: string, predicate: () => boolean | Promise<boolean>, timeout = 20_000) {
  const deadline = Date.now() + timeout;
  while (Date.now() < deadline) {
    if (await predicate()) return;
    await new Promise((resolve) => setTimeout(resolve, 50));
  }
  throw new Error(`timed out waiting for ${label}`);
}

async function main() {
  fs.rmSync(STORE, { recursive: true, force: true });
  fs.mkdirSync(STORE, { recursive: true });
  const payloads: Buffer[] = [];
  const collector = http.createServer((request, response) => {
    const chunks: Buffer[] = [];
    request.on('data', (chunk) => chunks.push(chunk));
    request.on('end', () => {
      payloads.push(Buffer.concat(chunks));
      response.writeHead(200, { 'content-type': 'application/x-protobuf' });
      response.end();
    });
  });
  await new Promise<void>((resolve) => collector.listen(0, '127.0.0.1', resolve));
  const collectorPort = (collector.address() as { port: number }).port;
  const upstream = await startUpstream('echo', { firstDelayMs: 4_000 });
  const { server, baseUrl } = spawnServer('real', PORT, {
    ...realServerEnv('echo', upstream),
    AWAKEN_INGRESS: 'durable',
    AWAKEN_STORAGE_DIR: STORE,
    OTEL_EXPORTER_OTLP_ENDPOINT: `http://127.0.0.1:${collectorPort}`,
    OTEL_EXPORTER_OTLP_PROTOCOL: 'http/protobuf',
    OTEL_METRIC_EXPORT_INTERVAL: '200',
  });

  try {
    await waitForPort(PORT, 180_000, server);
    const chat = new Chat({
      id: THREAD,
      transport: new DefaultChatTransport({ api: `${baseUrl}/v1/ai-sdk/threads/${THREAD}/runs` }),
    });
    const run = chat.sendMessage({ text: 'commit exactly once after recovery' });
    await waitUntil('attempt A to enter provider inference', () => upstream.received >= 1);

    const database = `${STORE}/dispatch.db`;
    execFileSync(
      'sqlite3',
      [database, "PRAGMA busy_timeout=10000; UPDATE runtime_dispatch SET lease_until = 0 WHERE status = 'running'"],
      { timeout: 15_000 },
    );
    const recovery = await fetch(`${baseUrl}/v1/durable/threads/${THREAD}/reconcile`, { method: 'POST' });
    const recoveryBody = await recovery.text();
    assert.equal(recovery.status, 200, recoveryBody);
    const recovered = JSON.parse(recoveryBody) as { recovered?: string[] };
    const recoveredByRequest = recovered.recovered ?? [];
    assert.ok(
      recoveredByRequest.length <= 1,
      `one expired dispatch cannot produce multiple reconcile winners: ${recoveryBody}`,
    );

    // The process-level pool and the operator reconcile endpoint are deliberately
    // allowed to race for the same expired lease. Either may win the epoch bump;
    // an empty response therefore means the pool already claimed attempt B, not
    // that recovery failed. The externally observable proof is the second provider
    // attempt followed by a fenced settlement from attempt A.
    await waitUntil('replacement attempt B to call the provider', () => upstream.received >= 2);
    await run;
    await waitUntil(
      'stale attempt fenced metric export',
      () => payloads.some((body) => body.includes(Buffer.from('awaken.dispatch.commits.fenced'))),
      20_000,
    );
    assert.equal(
      upstream.received,
      2,
      'exactly the stale attempt and its replacement reached inference',
    );
    console.log('DISPATCH FENCED METRICS TS API E2E PASS: replacement committed once and the stale in-process attempt exported the fenced counter.');
  } finally {
    await stopServer(server);
    upstream.close();
    await new Promise<void>((resolve) => collector.close(() => resolve()));
    fs.rmSync(STORE, { recursive: true, force: true });
  }
}

main().catch((error) => {
  console.error('E2E FAIL:', error);
  process.exitCode = 1;
});
