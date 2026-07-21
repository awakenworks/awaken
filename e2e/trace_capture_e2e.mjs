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
import { spawnServer, stopServer, waitForPort, pass, startUpstream, realServerEnv } from './harness.mjs';
import {
  readSpans,
  assertValidIds,
  assertConnected,
  assertRouteCoverage,
  assertGenAiChain,
  assertToolSpan,
  assertPropagation,
  assertBackgroundLinked,
  assertDurableDispatch,
} from './trace_validate.mjs';

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

const PORT = Number(process.env.E2E_PORT ?? 38211);
const BASE = `http://127.0.0.1:${PORT}`;
const BETAS = ['managed-agents-2026-04-01'];
const FILE = `/tmp/awaken-trace-capture-${process.pid}.jsonl`;

// A fixed inbound W3C trace context for the propagation probe.
const TID = 'abcdef0123456789abcdef0123456789';
const SID = '0123456789abcdef';

// One fake 'echo' upstream, shared by every server spawn/restart in this file.
let upstream;

// Create a session and drive one user-message turn to completion.
async function createAndTurn(base, text, resources = []) {
  const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: base });
  const session = await client.beta.sessions.create({
    agent: 'assistant',
    environment_id: 'env_local',
    resources,
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

// The wire behavior reproducing each capture mode's model: the `statemachine`
// scenario drives the builtin `glob` tool, the `memory` scenario drives a
// background extraction sub-run — neither of which an `echo` reply produces.
const CAPTURE_BEHAVIOR = { echo: 'echo', statemachine: 'stateMachine', memory: 'memory' };

// Spawn `mode` with a collector-free trace file, drive one turn, stop, and return
// the captured spans (SIGINT force-flushes the SimpleSpanProcessor on shutdown).
// The MODEL runs over the real wire: `echo` is plain-mount (real mode), other modes
// keep their `AWAKEN_MODEL_MODE=<mode>` host config with the model swapped to the
// wire. Each call runs its own fake upstream reproducing that mode's behavior.
// Background work is awaited through its observable resource effect rather than a
// timing guess, so a captured aux span proves the governed binding actually ran.
async function captureTurn(mode, port, file, text, { extraEnv = {}, settleMs = 0 } = {}) {
  fs.rmSync(file, { force: true });
  const behavior = CAPTURE_BEHAVIOR[mode] ?? 'echo';
  const up = await startUpstream(behavior);
  const realEnv =
    mode === 'echo' ? realServerEnv(behavior, up) : realServerEnv(behavior, up, { mode });
  const { server } = spawnServer(mode === 'echo' ? 'real' : mode, port, {
    AWAKEN_TRACE_FILE: file,
    ...extraEnv,
    ...realEnv,
  });
  try {
    await waitForPort(port);
    const base = `http://127.0.0.1:${port}`;
    let memoryStore;
    let resources = [];
    if (mode === 'memory') {
      const created = await fetch(`${base}/v1/memory_stores`, {
        method: 'POST',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify({ name: 'trace-memory' }),
      });
      assert.equal(created.status, 200, 'trace memory store created');
      memoryStore = await created.json();
      resources = [{ type: 'memory_store', memory_store_id: memoryStore.id, mount_path: '/memory' }];
    }
    await createAndTurn(base, text, resources);
    if (memoryStore) {
      let extracted = false;
      for (let i = 0; i < 30; i += 1) {
        const response = await fetch(`${base}/v1/memory_stores/${memoryStore.id}/memories`);
        assert.equal(response.status, 200, 'trace memory store remains readable');
        const page = await response.json();
        if ((page.data ?? []).length > 0) {
          extracted = true;
          break;
        }
        await sleep(100);
      }
      assert.ok(extracted, 'background extraction committed to the bound MemoryStore');
    } else if (settleMs) {
      await sleep(settleMs);
    }
    await stopServer(server);
    return readSpans(file);
  } finally {
    await stopServer(server).catch(() => {});
    up.close();
    fs.rmSync(file, { force: true });
  }
}

// Spawn echo mode with durable ingress + the standing dispatch daemon, submit a
// background run (admitted by the request, drained out of band by the daemon), let
// it drain, then return the captured spans.
async function captureDurable(port, file, storeDir) {
  fs.rmSync(file, { force: true });
  fs.rmSync(storeDir, { recursive: true, force: true });
  fs.mkdirSync(storeDir, { recursive: true });
  const { server } = spawnServer('real', port, {
    AWAKEN_TRACE_FILE: file,
    AWAKEN_INGRESS: 'durable',
    AWAKEN_DISPATCH_DAEMON: '1',
    AWAKEN_STORAGE_DIR: storeDir,
    ...realServerEnv('echo', upstream),
  });
  const base = `http://127.0.0.1:${port}`;
  try {
    await waitForPort(port);
    const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: base });
    const session = await client.beta.sessions.create({
      agent: 'assistant',
      environment_id: 'env_local',
      betas: BETAS,
    });
    const res = await fetch(`${base}/v1/durable/threads/${session.id}/submit_background`, {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ text: 'DURABLE-TRACE' }),
    });
    assert.equal(res.status, 200, 'background submit accepted');
    await sleep(3000); // let the daemon drain + execute the run
    await stopServer(server);
    return readSpans(file);
  } finally {
    await stopServer(server).catch(() => {});
    fs.rmSync(file, { force: true });
    fs.rmSync(storeDir, { recursive: true, force: true });
  }
}

async function main() {
  fs.rmSync(FILE, { force: true });
  upstream = await startUpstream('echo');
  const { server } = spawnServer('real', PORT, { AWAKEN_TRACE_FILE: FILE, ...realServerEnv('echo', upstream) });
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

    // 4) Spawn boundary: a background memory-extraction sub-run stays on the turn's
    //    trace via an `aux.background` span (memory mode drives the extraction).
    const memSpans = await captureTurn('memory', PORT + 2, `${FILE}.mem`, 'remember fact-sky', {
      settleMs: 1500,
    });
    assertValidIds(memSpans);
    assertConnected(memSpans);
    assertBackgroundLinked(memSpans);
    pass('spawn boundary: background aux run linked to the turn trace (aux.background)');

    // 5) Durable boundary: a background-submitted run drained by the daemon
    //    continues the submit request's trace via a `wake.dispatch` span.
    const durSpans = await captureDurable(PORT + 3, `${FILE}.dur`, `/tmp/awaken-trace-dur-${process.pid}`);
    assertValidIds(durSpans);
    assertConnected(durSpans);
    const wake = assertDurableDispatch(durSpans);
    pass(`durable boundary: wake.dispatch continues the submit trace (${wake.trace_id.slice(0, 8)}…)`);

    console.log('E2E PASS: captured traces match the OTel GenAI conventions and propagate correctly.');
    process.exitCode = 0;
  } catch (err) {
    console.error('E2E FAIL:', err);
    process.exitCode = 1;
  } finally {
    await stopServer(server).catch(() => {});
    upstream.close();
    fs.rmSync(FILE, { force: true });
  }
}

main();
