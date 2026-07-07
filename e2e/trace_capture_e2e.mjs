// Distributed-tracing end-to-end: drive real scenarios through the instrumented
// server with a collector-free span sink (`AWAKEN_TRACE_FILE`), then assert the
// captured span trees against the OTel GenAI conventions and W3C `traceparent`
// propagation. No OTLP collector and no API key: the `echo` model drives a full
// turn, so the whole ingress → session → runtime → inference chain is exercised
// deterministically.
//
// Run: (from e2e/)  node trace_capture_e2e.mjs

import assert from 'node:assert/strict';
import fs from 'node:fs';
import Anthropic from '@anthropic-ai/sdk';
import { spawnServer, stopServer, waitForPort, pass } from './harness.mjs';
import {
  readSpans,
  assertValidIds,
  assertConnected,
  assertRouteCoverage,
  assertGenAiChain,
  assertToolSpan,
  assertPropagation,
} from './trace_validate.mjs';

const PORT = Number(process.env.E2E_PORT ?? 38211);
const BASE = `http://127.0.0.1:${PORT}`;
const BETAS = ['managed-agents-2026-04-01'];
const FILE = `/tmp/awaken-trace-capture-${process.pid}.jsonl`;

// A fixed inbound W3C trace context for the propagation probe.
const TID = 'abcdef0123456789abcdef0123456789';
const SID = '0123456789abcdef';

// Create a session and drive one user-message turn to completion.
async function createAndTurn(base, text) {
  const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: base });
  const session = await client.beta.sessions.create({
    agent: 'assistant',
    environment_id: 'env_local',
    betas: BETAS,
  });
  const send = await fetch(`${base}/v1/sessions/${session.id}/events`, {
    method: 'POST',
    headers: { 'content-type': 'application/json', 'anthropic-beta': BETAS[0] },
    body: JSON.stringify({
      events: [{ type: 'user.message', content: [{ type: 'text', text }] }],
    }),
  });
  assert.equal(send.status, 200, 'turn accepted');
  return session;
}

// Spawn `mode` with a collector-free trace file, drive one turn, stop, and return
// the captured spans (SIGINT force-flushes the SimpleSpanProcessor on shutdown).
async function captureTurn(mode, port, file, text) {
  fs.rmSync(file, { force: true });
  const { server } = spawnServer(mode, port, { AWAKEN_TRACE_FILE: file });
  try {
    await waitForPort(port);
    await createAndTurn(`http://127.0.0.1:${port}`, text);
    await stopServer(server);
    return readSpans(file);
  } finally {
    await stopServer(server).catch(() => {});
    fs.rmSync(file, { force: true });
  }
}

async function main() {
  fs.rmSync(FILE, { force: true });
  const { server } = spawnServer('echo', PORT, { AWAKEN_TRACE_FILE: FILE });
  try {
    await waitForPort(PORT);

    // 1) Create a session, then drive a turn (echo model): this is the deep
    //    ingress → sessions.events.send → invoke_agent → chat chain.
    await createAndTurn(BASE, 'hello trace');
    pass('drove a turn through the echo model (ingress → runtime → inference)');

    // 2) Propagation probe: a request carrying an upstream traceparent must
    //    continue that trace (same trace id, parent = the upstream span id).
    const models = await fetch(`${BASE}/v1/models`, {
      headers: { traceparent: `00-${TID}-${SID}-01`, 'anthropic-beta': BETAS[0] },
    });
    assert.equal(models.status, 200, 'models listed');
    pass('sent a request with an inbound W3C traceparent');

    // Stop the server so the SimpleSpanProcessor has flushed every finished span
    // to the file (shutdown() force-flushes on SIGINT), then read them back.
    await stopServer(server);
    const spans = readSpans(FILE);
    pass(`captured ${spans.length} spans`);

    assertValidIds(spans);
    pass('all spans carry well-formed 32-hex trace ids / 16-hex span ids');

    assertConnected(spans);
    pass('no internal span dangles; every child shares its parent trace');

    assertRouteCoverage(spans, [
      '/v1/sessions',
      '/v1/sessions/{id}/events',
      '/v1/models',
    ]);
    pass('every driven REST route produced an ingress http.request span');

    const chat = assertGenAiChain(spans);
    pass(`OTel GenAI chain intact: sessions.events.send → invoke_agent → chat (${chat.attributes['gen_ai.request.model']})`);

    assertPropagation(spans, '/v1/models', TID, SID);
    pass('inbound W3C traceparent continued the upstream trace across the ingress boundary');

    // 3) Tool scenario: a separate server whose model drives the builtin `glob`
    //    tool inline, so the OTel GenAI `execute_tool {tool}` span is captured
    //    under `invoke_agent` on the same trace (echo drives no tools).
    const toolSpans = await captureTurn('statemachine', PORT + 1, `${FILE}.tool`, 'go');
    assertValidIds(toolSpans);
    assertConnected(toolSpans);
    const glob = assertToolSpan(toolSpans, 'glob');
    pass(`OTel GenAI tool span intact: invoke_agent → "${glob.name}" (call ${glob.attributes['gen_ai.tool.call.id']})`);

    console.log('E2E PASS: captured traces match the OTel GenAI conventions and propagate correctly.');
    process.exitCode = 0;
  } catch (err) {
    console.error('E2E FAIL:', err);
    process.exitCode = 1;
  } finally {
    await stopServer(server).catch(() => {});
    fs.rmSync(FILE, { force: true });
  }
}

main();
