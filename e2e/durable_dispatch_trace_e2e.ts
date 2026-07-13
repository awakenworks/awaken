// Distributed-trace continuity across the DURABLE DISPATCH boundary — TypeScript.
//
// A background run is admitted with an inbound W3C `traceparent`, persisted on the
// durable queue, and drained LATER by the dispatch daemon (a different task, in
// production a different node). The `tracing` span context does not survive that
// queue hop, so without an explicit relay the drained run would emit a fresh,
// detached trace. The worker (`awaken-run-ingress` `drive_claimed`) rebuilds a
// `wake.dispatch` span whose remote parent is the persisted `traceparent`, so
// `runtime.run` nests under the SUBMITTER's trace (same trace_id).
//
// This is the TypeScript e2e for that gap: it drives the real server + daemon,
// controls the trace_id via an explicit inbound header, and proves every worker-side
// span carries THAT trace_id and chains back to the submit ingress span.
//
// Run (from e2e/):  node durable_dispatch_trace_e2e.ts

import assert from 'node:assert/strict';
import fs from 'node:fs';
import { mkdtempSync } from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';
import Anthropic from '@anthropic-ai/sdk';
// The shared harness + span validators are plain ESM JS; imported untyped (any).
import {
  spawnServer,
  stopServer,
  waitForPort,
  pass,
  startUpstream,
  realServerEnv,
} from './harness.mjs';
import { readSpans, assertValidIds, assertConnected } from './trace_validate.mjs';

// One finished OTLP span as captured to the trace file (see trace_validate.mjs).
interface Span {
  name: string;
  trace_id: string;
  span_id: string;
  parent_span_id: string | null;
  attributes?: Record<string, unknown>;
}

const sleep = (ms: number): Promise<void> => new Promise((r) => setTimeout(r, ms));

const PORT = Number(process.env.E2E_PORT ?? 39647);
const BASE = `http://127.0.0.1:${PORT}`;
const BETAS = ['managed-agents-2026-04-01'];
const FILE = `/tmp/awaken-dispatch-trace-ts-${process.pid}.jsonl`;

// A fixed inbound W3C trace context the submitter continues; the daemon-drained run
// must stay on this same trace_id rather than starting a detached root.
const TID = 'aa11bb22cc33dd44ee55ff6677889900';
const SID = '1122334455667788';

// Walk a span's ancestor chain (nearest-first), stopping at a parent absent from the
// captured set.
function ancestors(span: Span, byId: Map<string, Span>): Span[] {
  const chain: Span[] = [];
  let cur: Span | undefined = span;
  const seen = new Set<string>();
  while (cur && cur.parent_span_id != null && !seen.has(cur.span_id)) {
    seen.add(cur.span_id);
    const parent = byId.get(cur.parent_span_id);
    if (!parent) break;
    chain.push(parent);
    cur = parent;
  }
  return chain;
}

async function messages(thread: string): Promise<Array<{ role: string; text?: string }>> {
  const res = await fetch(`${BASE}/v1/durable/threads/${thread}/messages`);
  if (res.status !== 200) throw new Error(`messages ${res.status}: ${await res.text()}`);
  return ((await res.json()) as { messages?: Array<{ role: string; text?: string }> }).messages ?? [];
}

// Poll committed truth until the daemon has drained + driven the run (we do not drive
// it — the pool does).
async function waitForAssistant(thread: string, timeoutMs = 20_000): Promise<void> {
  const deadline = Date.now() + timeoutMs;
  for (;;) {
    const msgs = await messages(thread);
    if (msgs.some((m) => m.role === 'Assistant' && (m.text ?? '').length > 0)) return;
    if (Date.now() > deadline) {
      throw new Error(`timed out waiting for the daemon to drive the run; saw ${JSON.stringify(msgs)}`);
    }
    await sleep(150);
  }
}

async function main(): Promise<void> {
  fs.rmSync(FILE, { force: true });
  const storeDir = mkdtempSync(path.join(tmpdir(), 'awaken-dispatch-trace-ts-'));
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

    const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: BASE });
    const session = await client.beta.sessions.create({
      agent: 'assistant',
      environment_id: 'env_local',
      betas: BETAS,
    });
    pass(`created durable thread ${session.id}`);

    // Submit a BACKGROUND run carrying an explicit inbound trace context. It is
    // admitted here and drained later by the daemon (out of band) — the true durable
    // queue boundary, not the synchronous turn path.
    const res = await fetch(`${BASE}/v1/durable/threads/${session.id}/submit_background`, {
      method: 'POST',
      headers: { 'content-type': 'application/json', traceparent: `00-${TID}-${SID}-01` },
      body: JSON.stringify({ text: 'DURABLE-DISPATCH-TRACE-TS' }),
    });
    assert.equal(res.status, 200, 'background submit accepted');
    const body = (await res.json()) as { queued?: boolean; run_id?: string };
    assert.equal(body.queued, true, 'run was queued (not driven inline)');
    pass(`submitted background run ${body.run_id} with inbound trace_id=${TID.slice(0, 8)}… (daemon will drain)`);

    await waitForAssistant(session.id);
    pass('daemon drained + drove the queued run to a committed reply');

    // Let the span processor flush, then SIGINT force-flushes every finished span.
    await sleep(300);
    await stopServer(server);
    const spans = readSpans(FILE) as Span[];
    pass(`captured ${spans.length} spans`);

    assertValidIds(spans);
    assertConnected(spans);
    pass('all spans have well-formed ids and no internal span dangles across traces');

    const byId = new Map<string, Span>(spans.map((s) => [s.span_id, s]));

    // (A) The submitter's admission span continued the inbound trace.
    const submit = spans.find(
      (s) =>
        s.name === 'http.request' &&
        String(s.attributes?.['http.route'] ?? '').endsWith('submit_background'),
    );
    assert.ok(submit, 'no submit_background ingress span captured');
    assert.equal(submit.trace_id, TID, `submit ingress not on the inbound trace (${submit.trace_id})`);
    assert.equal(submit.parent_span_id, SID, `submit ingress ignored the inbound parent (${submit.parent_span_id})`);
    pass(`submit ingress continued the inbound trace: trace_id=${submit.trace_id}, parent=${submit.parent_span_id}`);

    // (B) The worker-side dispatch relay span shares the submitter's trace and chains
    //     back into the submit ingress span (the cross-queue hop).
    const wake = spans.find((s) => s.name === 'wake.dispatch');
    assert.ok(wake, 'no wake.dispatch span; the durable trace relay is missing (detached run)');
    assert.equal(
      wake.trace_id,
      submit.trace_id,
      `DETACHED: wake.dispatch trace_id=${wake.trace_id} != submitter ${submit.trace_id}`,
    );
    const wakeChain = [wake, ...ancestors(wake, byId)];
    assert.ok(
      wakeChain.some((s) => s.span_id === submit.span_id),
      `wake.dispatch not chained back to submit; chain: ${wakeChain.map((s) => s.name).join(' -> ')}`,
    );
    pass(`wake.dispatch nests under submit on the SAME trace (trace_id=${wake.trace_id})`);

    // (C) The daemon-driven run is not a fresh trace: same trace_id, chained through
    //     wake.dispatch back to the submit ingress span.
    const run = spans.find(
      (s) => s.name === 'runtime.run' && ancestors(s, byId).some((a) => a.span_id === wake.span_id),
    );
    assert.ok(run, 'wake.dispatch drove no runtime.run span under the relay');
    assert.equal(run.trace_id, TID, `DETACHED: runtime.run trace_id=${run.trace_id} != submitter ${TID}`);
    const runChain = ancestors(run, byId);
    assert.ok(
      runChain.some((s) => s.span_id === submit.span_id),
      `runtime.run chain does not lead back to submit; chain: ${runChain.map((s) => s.name).join(' -> ')}`,
    );
    pass(`runtime.run is on the submitter's trace and chains back through wake.dispatch -> submit`);

    // (D) The whole worker-side run subtree shares the submitter's trace_id (no split).
    const subtree = spans.filter(
      (s) => s.span_id === run.span_id || ancestors(s, byId).some((a) => a.span_id === run.span_id),
    );
    for (const s of subtree) {
      assert.equal(s.trace_id, TID, `span ${s.name} split onto a different trace (${s.trace_id})`);
    }
    pass(`all ${subtree.length} worker-side run-subtree spans share the submitter's trace_id (no split)`);

    // The headline invariant, once more against detached roots.
    assert.equal(submit.trace_id, wake.trace_id);
    assert.equal(wake.trace_id, run.trace_id);
    console.log(
      '\nDURABLE DISPATCH TRACE E2E (TS) PASS: a background run drained by the daemon nests under the submitter\'s trace (same trace_id), not a fresh detached trace.',
    );
    pass('durable dispatch trace propagation (TypeScript)');
    process.exitCode = 0;
  } catch (err) {
    console.error('\nDURABLE DISPATCH TRACE E2E (TS) FAIL:', err);
    process.exitCode = 1;
  } finally {
    await stopServer(server).catch(() => {});
    upstream.close();
    fs.rmSync(FILE, { force: true });
    fs.rmSync(storeDir, { recursive: true, force: true });
  }
}

main();
