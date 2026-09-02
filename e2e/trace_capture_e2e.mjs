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
import {
  cleanupFixtureTree,
  pass,
  realServerEnv,
  scenarioMemoryStore,
  spawnServer,
  startUpstream,
  stopServer,
  waitForPort,
  waitForSessionEventReceipt,
  waitForValue,
} from './harness.mjs';
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
const MEMORY_HEADERS = { 'anthropic-beta': 'agent-memory-2026-07-22' };
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
  const receipt = await client.beta.sessions.events.send(session.id, {
    events: [{ type: 'user.message', content: [{ type: 'text', text }] }],
    betas: BETAS,
  });
  // Trace-capture decision table: C1=official SDK create/send returns an exact
  // durable User receipt; C2=Run reconciliation may finish after admission;
  // C3=the caller will stop/flush the span processor. E1=anchor on the exact
  // processed receipt; E2=observe its later agent.message + aggregate idle;
  // E3=only then allow C3 so the completed model/tool span tree is capturable.
  // K: the SDK still traverses the served route, while the canonical receipt
  // adapter only reads official history and never drives the Runtime.
  // Decision T1 C1&&!C2=>keep observing; T2 C1+C2=>E1+E2; T3 T2+C3=>E3.
  const receiptId = receipt.data?.[0]?.id;
  assert.equal(typeof receiptId, 'string', 'T1 exact trace-turn User Event receipt');
  await waitForSessionEventReceipt(
    client,
    session.id,
    receiptId,
    BETAS,
    ({ delta }) => delta.some((event) => event.type === 'agent.message')
      && delta.some((event) => event.type === 'session.status_idle'),
    `T1 trace turn ${JSON.stringify(text)} to commit before span flush`,
  );
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
// The resource effect proves the governed binding ran; process shutdown then
// joins the mounted ServiceLifecycle so the background parent span also ends
// before the trace exporter is flushed. Neither boundary relies on a timing wait.
async function captureTurn(mode, port, file, text, { extraEnv = {} } = {}) {
  fs.rmSync(file, { force: true });
  const behavior = CAPTURE_BEHAVIOR[mode] ?? 'echo';
  const up = await startUpstream(behavior);
  const realEnv =
    mode === 'echo' ? realServerEnv(behavior, up) : realServerEnv(behavior, up, { mode });
  const { server } = spawnServer(mode === 'echo' ? 'real' : mode, port, {
    AWAKEN_TRACE_FILE: file,
    ...realEnv,
    ...extraEnv,
  });
  try {
    await waitForPort(port);
    const base = `http://127.0.0.1:${port}`;
    let memoryStore;
    let resources = [];
    if (mode === 'memory') {
      const memoryClient = new Anthropic({ apiKey: 'e2e-dummy', baseURL: base });
      memoryStore = await scenarioMemoryStore(memoryClient, MEMORY_HEADERS);
      resources = [{ type: 'memory_store', memory_store_id: memoryStore.id }];
    }
    await createAndTurn(base, text, resources);
    if (memoryStore) {
      let extracted = false;
      for (let i = 0; i < 30; i += 1) {
        const response = await fetch(`${base}/v1/memory_stores/${memoryStore.id}/memories`, {
          headers: MEMORY_HEADERS,
        });
        assert.equal(response.status, 200, 'trace memory store remains readable');
        const page = await response.json();
        if ((page.data ?? []).length > 0) {
          extracted = true;
          break;
        }
        await sleep(100);
      }
      assert.ok(extracted, 'background extraction committed to the bound MemoryStore');
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
  cleanupFixtureTree(storeDir);
  fs.mkdirSync(storeDir, { recursive: true });
  const { server } = spawnServer('real', port, {
    AWAKEN_TRACE_FILE: file,
    SESSION_DEPLOYMENT_INGRESS: 'durable',
    AWAKEN_DISPATCH_DAEMON: '1',
    SESSION_DEPLOYMENT_STORAGE_DIR: storeDir,
    ...realServerEnv('echo', upstream),
  });
  const base = `http://127.0.0.1:${port}`;
  try {
    await waitForPort(port);
    const thread = `durable-trace-capture-${process.pid}`;
    // D0 ownership: generic background ingress owns an ordinary Runtime Thread;
    // Managed Session roots use their Session-owned reservation path instead.
    const res = await fetch(`${base}/v1/durable/threads/${thread}/submit_background`, {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ text: 'DURABLE-TRACE' }),
    });
    assert.equal(res.status, 200, 'background submit accepted');
    const admitted = await res.json();
    assert.equal(admitted.queued, true, 'D1 durable trace Run is queued');
    assert.equal(typeof admitted.run_id, 'string', 'D1 durable trace Run id');
    // Durable-flush decision table: C1=submit_background durably queues a Run;
    // C2=the daemon later claims and drives it; C3=an Assistant message commits.
    // E1=only C3 authorizes server stop/span flush. K: HTTP 200 and elapsed time
    // prove C1 only; committed Thread history is the existing C2/C3 authority.
    // Decision D1 C1&&!C3=>keep observing; D2 C1+C2+C3=>E1.
    await waitForValue(
      async () => {
        const response = await fetch(`${base}/v1/durable/threads/${thread}/messages`);
        assert.equal(response.status, 200, 'D1 durable trace messages remain readable');
        return (await response.json()).messages ?? [];
      },
      (messages) => messages.some(
        (message) => message.role === 'Assistant' && (message.text ?? '').length > 0,
      ),
      'D1 durable trace daemon to commit an Assistant reply before span flush',
      { timeoutMs: 20_000, pollMs: 150 },
    );
    await stopServer(server);
    return readSpans(file);
  } finally {
    await stopServer(server).catch(() => {});
    fs.rmSync(file, { force: true });
    cleanupFixtureTree(storeDir);
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

    // 4) Spawn boundary decision table. C1 the admitted User Event persists its
    // traceparent on one durable RunDispatch; C2 the daemon owns terminal replay;
    // C3 the Worker-owned post-commit observer enqueues Memory extraction; C4 the
    // MemoryStore effect can become visible before its detached task returns;
    // C5 graceful process shutdown joins the mounted Runtime background drain.
    // E1 aux.background descends through wake.dispatch to sessions.events.send;
    // E2 the extractor runtime.run is below aux.background; E3 one Memory effect
    // commits; E4 both child and parent spans finish before trace flush. K:
    // RunDispatch is the sole causal relay, MemoryStore is the effect authority,
    // and ServiceLifecycle is the sole bounded shutdown owner—there is no sleep
    // or second completion ledger. D1: C1+C2+C3+C4+C5 => E1+E2+E3+E4.
    const memoryRoot = `/tmp/awaken-trace-memory-${process.pid}`;
    cleanupFixtureTree(memoryRoot);
    let memSpans;
    try {
      memSpans = await captureTurn('memory', PORT + 2, `${FILE}.mem`, 'remember fact-sky', {
        extraEnv: {
          SESSION_DEPLOYMENT_INGRESS: 'durable',
          AWAKEN_DISPATCH_DAEMON: '1',
          SESSION_DEPLOYMENT_STORAGE_DIR: memoryRoot,
        },
      });
    } finally {
      cleanupFixtureTree(memoryRoot);
    }
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
