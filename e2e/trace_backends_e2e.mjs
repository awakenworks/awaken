// Live-OTLP backend validation: drive real traffic through the instrumented
// server exporting over OTLP, then assert the captured spans landed correctly in
// BOTH Phoenix and Jaeger (the collector fans one stream out to each). This is the
// live counterpart to trace_capture_e2e.mjs (which asserts the collector-free
// file sink); it proves the OTLP export path and cross-backend consistency.
//
// Requires the stack up:  docker compose -f e2e/phoenix/docker-compose.yml up -d --wait
// Self-skips (green) when Jaeger/Phoenix are not reachable, so the default suite
// stays runnable without Docker.
//
// Run: (from e2e/)  node trace_backends_e2e.mjs

import assert from 'node:assert/strict';
import { spawnServer, stopServer, waitForPort, pass } from './harness.mjs';

const COLLECTOR = 'http://127.0.0.1:4318/v1/traces';
const JAEGER = 'http://127.0.0.1:16686';
const PHOENIX = 'http://127.0.0.1:6006';
const PORT = Number(process.env.E2E_PORT ?? 38221);
const BASE = `http://127.0.0.1:${PORT}`;
const BETAS = ['managed-agents-2026-04-01'];

// Per-run identity so queries isolate THIS run from earlier traces in the shared
// backends: a unique service name (Jaeger filters on it) and a unique inbound
// trace id (the propagation probe; both backends are keyed on it).
const PID = process.pid;
const SVC = `awaken-trace-e2e-${PID}`;
const HEX8 = PID.toString(16).padStart(8, '0').slice(-8);
const TID = (HEX8 + '0'.repeat(24)).slice(0, 32);
const SIDP = (HEX8 + '0'.repeat(8)).slice(0, 16);

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

async function reachable(url) {
  try {
    const r = await fetch(url, { signal: AbortSignal.timeout(2000) });
    return r.ok;
  } catch {
    return false;
  }
}

async function poll(label, fn, { tries = 30, delay = 1000 } = {}) {
  let last;
  for (let i = 0; i < tries; i++) {
    try {
      const v = await fn();
      if (v) return v;
    } catch (e) {
      last = e;
    }
    await sleep(delay);
  }
  throw new Error(`timed out waiting for ${label}${last ? `: ${last.message}` : ''}`);
}

async function driveTraffic() {
  const { server } = spawnServer('statemachine', PORT, {
    OTEL_EXPORTER_OTLP_TRACES_ENDPOINT: COLLECTOR,
    OTEL_EXPORTER_OTLP_TRACES_PROTOCOL: 'http/protobuf',
    OTEL_SERVICE_NAME: SVC,
  });
  await waitForPort(PORT);
  // A turn drives invoke_agent -> chat + execute_tool (glob, inline).
  const create = await fetch(`${BASE}/v1/sessions`, {
    method: 'POST',
    headers: { 'content-type': 'application/json', 'anthropic-beta': BETAS[0] },
    body: JSON.stringify({ agent: 'assistant', environment_id: 'env_local' }),
  });
  const session = await create.json();
  const send = await fetch(`${BASE}/v1/sessions/${session.id}/events`, {
    method: 'POST',
    headers: { 'content-type': 'application/json', 'anthropic-beta': BETAS[0] },
    body: JSON.stringify({
      events: [{ type: 'user.message', content: [{ type: 'text', text: 'go' }] }],
    }),
  });
  assert.equal(send.status, 200, 'turn accepted');
  // Propagation probe: an inbound traceparent we control, so both backends can be
  // keyed on TID and the continuation asserted.
  const models = await fetch(`${BASE}/v1/models`, {
    headers: { traceparent: `00-${TID}-${SIDP}-01`, 'anthropic-beta': BETAS[0] },
  });
  assert.equal(models.status, 200, 'models listed');
  // Stop so the batch span processor force-flushes on shutdown.
  await stopServer(server);
}

// ---- Jaeger --------------------------------------------------------------

async function jaegerTrace(traceId) {
  const r = await fetch(`${JAEGER}/api/traces/${traceId}`);
  if (!r.ok) return null;
  const body = await r.json();
  return body.data?.[0] ?? null;
}

async function validateJaeger() {
  // Operations (with SpanKind) for our isolated service.
  const ops = await poll('jaeger operations', async () => {
    const r = await fetch(`${JAEGER}/api/operations?service=${SVC}`);
    if (!r.ok) return null;
    const body = await r.json();
    const names = (body.data ?? []).map((o) => o.name);
    return names.includes('invoke_agent') && names.some((n) => n.startsWith('execute_tool'))
      ? body.data
      : null;
  });
  const kind = (name) => ops.find((o) => o.name === name)?.spanKind;
  assert.equal(kind('http.request'), 'server', 'http.request should be SpanKind server in Jaeger');
  assert.equal(kind('invoke_agent'), 'internal', 'invoke_agent should be SpanKind internal');
  const chatOp = ops.find((o) => o.name.startsWith('chat '));
  assert.ok(chatOp, 'no chat span operation in Jaeger');
  assert.equal(chatOp.spanKind, 'client', 'chat should be SpanKind client');
  const toolOp = ops.find((o) => o.name.startsWith('execute_tool '));
  assert.equal(toolOp.spanKind, 'internal', 'execute_tool should be SpanKind internal');
  pass(`Jaeger has the GenAI operations with correct SpanKinds (${chatOp.name}, ${toolOp.name})`);

  // Fetch a full turn trace for this service and assert the tree + attributes.
  const turn = await poll('jaeger turn trace', async () => {
    const r = await fetch(`${JAEGER}/api/traces?service=${SVC}&limit=50&lookback=1h`);
    if (!r.ok) return null;
    const body = await r.json();
    return (body.data ?? []).find((t) =>
      t.spans.some((s) => s.operationName === 'invoke_agent'),
    );
  });
  const byId = new Map(turn.spans.map((s) => [s.spanID, s]));
  const parent = (s) => {
    const ref = (s.references ?? []).find((r) => r.refType === 'CHILD_OF');
    return ref ? byId.get(ref.spanID) : null;
  };
  const ancestors = (s) => {
    const chain = [];
    let cur = parent(s);
    while (cur) {
      chain.push(cur);
      cur = parent(cur);
    }
    return chain;
  };
  const tag = (s, k) => (s.tags ?? []).find((t) => t.key === k)?.value;

  const chat = turn.spans.find((s) => tag(s, 'gen_ai.operation.name') === 'chat');
  assert.ok(chat, 'no chat span in Jaeger turn trace');
  assert.ok(tag(chat, 'gen_ai.request.model'), 'chat span missing gen_ai.request.model in Jaeger');
  assert.equal(tag(chat, 'gen_ai.provider.name'), 'awaken', 'chat span missing gen_ai.provider.name');
  assert.ok(
    ancestors(chat).some((s) => s.operationName === 'invoke_agent'),
    'chat not under invoke_agent in Jaeger',
  );
  const tool = turn.spans.find((s) => tag(s, 'gen_ai.operation.name') === 'execute_tool');
  assert.ok(tool, 'no execute_tool span in Jaeger turn trace');
  assert.equal(tag(tool, 'gen_ai.tool.name'), 'glob', 'execute_tool span wrong gen_ai.tool.name');
  assert.ok(
    ancestors(tool).some((s) => s.operationName === 'invoke_agent'),
    'execute_tool not under invoke_agent in Jaeger',
  );
  pass('Jaeger turn trace: invoke_agent → chat + execute_tool glob, GenAI attributes intact');

  // Propagation: the trace we injected continues under our SID.
  const prop = await poll('jaeger propagation trace', () => jaegerTrace(TID));
  const root = prop.spans.find((s) => s.operationName === 'http.request');
  assert.ok(root, 'no ingress span on the injected trace in Jaeger');
  const ref = (root.references ?? []).find((r) => r.refType === 'CHILD_OF');
  assert.ok(ref && ref.spanID === SIDP, `inbound traceparent parent not honored in Jaeger (${ref?.spanID} != ${SIDP})`);
  pass(`Jaeger: inbound traceparent continued (trace ${TID.slice(0, 8)}…, parent ${SIDP})`);
  return TID;
}

// ---- Phoenix -------------------------------------------------------------

async function phoenixSpans() {
  const query =
    '{ projects { edges { node { spans { edges { node { name spanId parentId context { traceId } attributes } } } } } } }';
  const r = await fetch(`${PHOENIX}/graphql`, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify({ query }),
  });
  if (!r.ok) return null;
  const body = await r.json();
  const edges = body.data?.projects?.edges?.[0]?.node?.spans?.edges ?? [];
  return edges.map((e) => e.node);
}

// Phoenix nests dotted attribute keys ({gen_ai:{request:{model}}}); flatten back.
function flatten(obj, prefix = '', out = {}) {
  for (const [k, v] of Object.entries(obj ?? {})) {
    const key = prefix ? `${prefix}.${k}` : k;
    if (v && typeof v === 'object' && !Array.isArray(v)) flatten(v, key, out);
    else out[key] = v;
  }
  return out;
}

async function validatePhoenix() {
  const attrsOf = (s) => flatten(typeof s.attributes === 'string' ? JSON.parse(s.attributes) : s.attributes);

  // The same GenAI spans reached Phoenix, with gen_ai attributes.
  await poll('phoenix chat span', async () => {
    const spans = await phoenixSpans();
    if (!spans) return null;
    const chat = spans.find((s) => {
      const a = attrsOf(s);
      return a['gen_ai.operation.name'] === 'chat' && a['gen_ai.provider.name'] === 'awaken';
    });
    if (!chat) return null;
    const a = attrsOf(chat);
    assert.ok(a['gen_ai.request.model'], 'Phoenix chat span missing gen_ai.request.model');
    return chat;
  });
  await poll('phoenix execute_tool span', async () => {
    const spans = await phoenixSpans();
    return spans?.find((s) => attrsOf(s)['gen_ai.tool.name'] === 'glob') ?? null;
  });
  pass('Phoenix received the GenAI spans (chat + execute_tool glob) with gen_ai.* attributes');

  // Cross-backend: the same injected trace id is present in Phoenix too.
  await poll('phoenix propagation trace', async () => {
    const spans = await phoenixSpans();
    return spans?.some((s) => s.context?.traceId === TID) ? true : null;
  });
  pass(`Phoenix has the same injected trace ${TID.slice(0, 8)}… (collector fan-out consistent with Jaeger)`);
}

async function main() {
  if (!(await reachable(`${JAEGER}/api/services`)) || !(await reachable(`${PHOENIX}/v1/projects`))) {
    console.log('SKIP: Jaeger/Phoenix not reachable — bring up e2e/phoenix/docker-compose.yml to run this.');
    console.log('E2E PASS: trace-backends check skipped (no OTLP stack).');
    process.exitCode = 0;
    return;
  }
  try {
    await driveTraffic();
    pass('drove a turn + propagation probe through the OTLP exporter');
    const tid = await validateJaeger();
    await validatePhoenix();
    assert.equal(tid, TID);
    console.log('E2E PASS: OTLP traces landed correctly in both Phoenix and Jaeger, consistently.');
    process.exitCode = 0;
  } catch (err) {
    console.error('E2E FAIL:', err);
    process.exitCode = 1;
  }
}

main();
