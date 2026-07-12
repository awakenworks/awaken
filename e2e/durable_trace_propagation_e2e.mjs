// Distributed trace continuity across the DURABLE DISPATCH boundary.
//
// The observability gap this closes: a run submitted with a trace context is not
// driven inline — it is admitted, persisted on the durable queue, and drained
// later by the dispatch daemon (a different pool task, and in production a
// different node/process). The `tracing` span context does NOT survive that
// queue hop, so without an explicit relay the drained run would emit a FRESH,
// disconnected trace and the operator would lose the causal link from "who asked"
// to "what ran".
//
// The relay (crates/server/awaken-run-ingress/src/worker.rs `drive_claimed`):
// the admitting request captures its context as a W3C `traceparent`
// (`current_traceparent()`), persists it on the durable instruction, and the
// worker rebuilds a `wake.dispatch` span whose REMOTE PARENT is that persisted
// traceparent — so `runtime.run` (and its step spans) nest under the SUBMITTER's
// trace, same trace_id, even though the daemon drained it out of band.
//
// This test controls the trace_id by sending an explicit inbound `traceparent`
// header on the durable submit, then proves every worker-side span carries THAT
// trace_id and chains back to the submit ingress span — not a detached root.
//
// Run: (from e2e/)  node durable_trace_propagation_e2e.mjs

import assert from 'node:assert/strict';
import fs from 'node:fs';
import { mkdtempSync } from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';
import Anthropic from '@anthropic-ai/sdk';
import {
  spawnServer,
  stopServer,
  waitForPort,
  pass,
  startUpstream,
  realServerEnv,
} from './harness.mjs';
import { readSpans, assertValidIds, assertConnected } from './trace_validate.mjs';

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

// An unusual free port, away from the 383xx / 397xx ranges other suites bind.
const PORT = Number(process.env.E2E_PORT ?? 39642);
const BASE = `http://127.0.0.1:${PORT}`;
const BETAS = ['managed-agents-2026-04-01'];
const FILE = `/tmp/awaken-durable-trace-prop-${process.pid}.jsonl`;

// A fixed inbound W3C trace context: the SUBMITTER continues THIS upstream trace,
// and the whole point is that the daemon-drained run stays on this same trace_id.
const TID = 'aa11bb22cc33dd44ee55ff6677889900';
const SID = '1122334455667788';

// Walk a span's ancestor chain (nearest-first), stopping at a parent absent from
// the file. Self-contained so the nesting assertions read explicitly here.
function ancestors(span, byId) {
  const chain = [];
  let cur = span;
  const seen = new Set();
  while (cur && cur.parent_span_id != null && !seen.has(cur.span_id)) {
    seen.add(cur.span_id);
    const p = byId.get(cur.parent_span_id);
    if (!p) break;
    chain.push(p);
    cur = p;
  }
  return chain;
}

async function messages(thread) {
  const res = await fetch(`${BASE}/v1/durable/threads/${thread}/messages`);
  if (res.status !== 200) throw new Error(`messages ${res.status}: ${await res.text()}`);
  return (await res.json()).messages ?? [];
}

// Observe committed truth: poll until the daemon has drained + driven the run and
// an assistant reply is committed. We do not drive the run — the pool does.
async function waitForAssistant(thread, timeoutMs = 20_000) {
  const deadline = Date.now() + timeoutMs;
  for (;;) {
    const msgs = await messages(thread);
    const assistants = msgs.filter((m) => m.role === 'Assistant' && (m.text ?? '').length > 0);
    if (assistants.length >= 1) return assistants;
    if (Date.now() > deadline) {
      throw new Error(`timed out waiting for the daemon to drive the run; saw ${JSON.stringify(msgs)}`);
    }
    await sleep(150);
  }
}

async function main() {
  fs.rmSync(FILE, { force: true });
  const storeDir = mkdtempSync(path.join(tmpdir(), 'awaken-durable-trace-'));
  const upstream = await startUpstream('echo');
  const { server } = spawnServer('real', PORT, {
    AWAKEN_TRACE_FILE: FILE,
    AWAKEN_INGRESS: 'durable',
    AWAKEN_DISPATCH_DAEMON: '1',
    AWAKEN_STORAGE_DIR: storeDir,
    ...realServerEnv('echo', upstream),
  });
  try {
    await waitForPort(PORT);

    // A durable thread to submit into.
    const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: BASE });
    const session = await client.beta.sessions.create({
      agent: 'assistant',
      environment_id: 'env_local',
      betas: BETAS,
    });
    pass(`created durable thread ${session.id}`);

    // Submit a BACKGROUND run carrying an explicit inbound trace context. It is
    // admitted here and drained later by the dispatch daemon (out of band), so
    // this is the true durable-queue boundary — not the synchronous turn path.
    const res = await fetch(`${BASE}/v1/durable/threads/${session.id}/submit_background`, {
      method: 'POST',
      headers: {
        'content-type': 'application/json',
        traceparent: `00-${TID}-${SID}-01`,
      },
      body: JSON.stringify({ text: 'DURABLE-TRACE-CONTINUITY' }),
    });
    assert.equal(res.status, 200, 'background submit accepted');
    const body = await res.json();
    assert.equal(body.queued, true, 'run was queued (not driven inline)');
    pass(`submitted background run ${body.run_id} with inbound traceparent trace_id=${TID.slice(0, 8)}… (queued, daemon will drain it)`);

    // Let the daemon claim + drive the run to a committed reply.
    const assistants = await waitForAssistant(session.id);
    pass(`daemon drained + drove the run to completion (reply: ${JSON.stringify(assistants[0].text)})`);

    // Give the SimpleSpanProcessor a moment, then SIGINT force-flushes every
    // finished span to the file.
    await sleep(300);
    await stopServer(server);
    const spans = readSpans(FILE);
    pass(`captured ${spans.length} spans`);

    assertValidIds(spans);
    pass('all spans carry well-formed 32-hex trace ids / 16-hex span ids');

    assertConnected(spans);
    pass('no internal span dangles; every child shares its parent trace');

    const byId = new Map(spans.map((s) => [s.span_id, s]));

    // (A) The SUBMITTER's admission span continued the inbound trace: the
    //     submit_background ingress span roots on TID with parent = SID.
    const submit = spans.find(
      (s) =>
        s.name === 'http.request' &&
        (s.attributes?.['http.route'] ?? '').endsWith('submit_background'),
    );
    assert.ok(submit, 'no submit_background ingress span captured');
    assert.equal(
      submit.trace_id,
      TID,
      `submit ingress not on the inbound trace (${submit.trace_id} != ${TID})`,
    );
    assert.equal(
      submit.parent_span_id,
      SID,
      `submit ingress did not honor inbound traceparent parent (${submit.parent_span_id} != ${SID})`,
    );
    pass(`submitter's admission span continued the inbound trace: trace_id=${submit.trace_id}, parent=${submit.parent_span_id}`);

    // (B) The worker-side dispatch relay span shares the SUBMITTER's trace_id and
    //     is parented back into the submit ingress span — this is the cross-queue
    //     hop that would otherwise start a fresh trace.
    const wake = spans.find((s) => s.name === 'wake.dispatch');
    assert.ok(wake, 'no wake.dispatch span; the durable trace relay is missing (detached run)');
    assert.equal(
      wake.trace_id,
      submit.trace_id,
      `DETACHED TRACE: wake.dispatch trace_id=${wake.trace_id} != submitter trace_id=${submit.trace_id}`,
    );
    const wakeChain = [wake, ...ancestors(wake, byId)];
    assert.ok(
      wakeChain.some((s) => s.span_id === submit.span_id),
      `wake.dispatch not chained back to the submit ingress span; chain: ${wakeChain.map((s) => s.name).join(' -> ')}`,
    );
    pass(`wake.dispatch nests under the submit ingress span on the SAME trace (trace_id=${wake.trace_id})`);

    // (C) The actual run driven by the daemon (`runtime.run`) is NOT a fresh
    //     disconnected trace: same trace_id, and its parent chain leads through
    //     wake.dispatch back to the submit ingress span.
    const runsUnderWake = spans.filter(
      (s) =>
        s.name === 'runtime.run' &&
        ancestors(s, byId).some((a) => a.span_id === wake.span_id),
    );
    assert.ok(
      runsUnderWake.length > 0,
      'wake.dispatch drove no runtime.run span (the drained run produced no execution span under the relay)',
    );
    const run = runsUnderWake[0];
    assert.equal(
      run.trace_id,
      TID,
      `DETACHED TRACE: runtime.run trace_id=${run.trace_id} != submitter trace_id=${TID}`,
    );
    const runChain = ancestors(run, byId);
    assert.ok(
      runChain.some((s) => s.span_id === wake.span_id),
      `runtime.run not under wake.dispatch; chain: ${runChain.map((s) => s.name).join(' -> ')}`,
    );
    assert.ok(
      runChain.some((s) => s.span_id === submit.span_id),
      `runtime.run chain does not lead back to the submit ingress span; chain: ${runChain.map((s) => s.name).join(' -> ')}`,
    );
    pass(`runtime.run driven by the daemon is on the submitter's trace and chains back through wake.dispatch -> submit (trace_id=${run.trace_id})`);

    // (D) The whole worker-side span set (dispatch + run + any step/child spans on
    //     the run's subtree) shares the SUBMITTER's trace_id — no split.
    const runSubtree = spans.filter((s) => {
      if (s.span_id === run.span_id) return true;
      return ancestors(s, byId).some((a) => a.span_id === run.span_id);
    });
    for (const s of runSubtree) {
      assert.equal(
        s.trace_id,
        TID,
        `span ${s.name} on the run subtree split onto a different trace (${s.trace_id} != ${TID})`,
      );
    }
    pass(`all ${runSubtree.length} worker-side run-subtree spans share the submitter's trace_id (no split, no detached root)`);

    // The headline invariant, stated once more against detached roots.
    assert.equal(submit.trace_id, wake.trace_id);
    assert.equal(wake.trace_id, run.trace_id);
    pass('CONTINUITY HOLDS: submit == wake.dispatch == runtime.run trace_id across the durable dispatch boundary');

    console.log('\nDURABLE TRACE PROPAGATION E2E PASS: a background run drained by the dispatch daemon nests under the submitter\'s trace (same trace_id), not a fresh disconnected trace.');
    pass('durable dispatch trace propagation');
    process.exitCode = 0;
  } catch (err) {
    console.error('\nDURABLE TRACE PROPAGATION E2E FAIL:', err);
    process.exitCode = 1;
  } finally {
    await stopServer(server).catch(() => {});
    upstream.close();
    fs.rmSync(FILE, { force: true });
    fs.rmSync(storeDir, { recursive: true, force: true });
  }
}

main();
