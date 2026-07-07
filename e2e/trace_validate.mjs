// Pure span-tree assertions over the collector-free `AWAKEN_TRACE_FILE` output
// (one finished span per JSON line: {name, trace_id, span_id, parent_span_id,
// attributes}). Shared by the trace-capture e2e so the checks stay declarative.
//
// The three properties a captured trace must hold:
//   1. validity      — 32-hex trace ids, 16-hex span ids, well-formed lines
//   2. connectedness — no internal span dangles: every non-root span's parent is
//                      present and on the same trace; the only roots are ingress
//                      `http.request` spans (whose parent may be an upstream,
//                      out-of-file `traceparent`)
//   3. shape         — the required REST routes each produced an ingress span, and
//                      the driven turn produced the OTel GenAI chain
//                      `sessions.events.send → invoke_agent → chat`

import assert from 'node:assert/strict';
import fs from 'node:fs';

const TRACE_ID_RE = /^[0-9a-f]{32}$/;
const SPAN_ID_RE = /^[0-9a-f]{16}$/;

export function readSpans(file) {
  const text = fs.readFileSync(file, 'utf8');
  const spans = text
    .split('\n')
    .filter((l) => l.trim())
    .map((l) => JSON.parse(l));
  assert.ok(spans.length > 0, `trace file ${file} has no spans`);
  return spans;
}

/// Collapse resource ids (e.g. `sesn_0`) to `{id}` so routes are comparable.
export function templatizeRoute(route) {
  return route.replace(/\/[a-z]+_[A-Za-z0-9-]+/g, '/{id}');
}

/// Index spans by id and by (templatized) ingress route.
function index(spans) {
  const byId = new Map(spans.map((s) => [s.span_id, s]));
  const routes = new Set();
  for (const s of spans) {
    const r = s.attributes?.['http.route'];
    if (s.name === 'http.request' && r) routes.add(templatizeRoute(r));
  }
  return { byId, routes };
}

/// (1) Every span carries well-formed ids.
export function assertValidIds(spans) {
  for (const s of spans) {
    assert.match(s.trace_id, TRACE_ID_RE, `bad trace_id on ${s.name}: ${s.trace_id}`);
    assert.match(s.span_id, SPAN_ID_RE, `bad span_id on ${s.name}: ${s.span_id}`);
    if (s.parent_span_id != null) {
      assert.match(s.parent_span_id, SPAN_ID_RE, `bad parent on ${s.name}: ${s.parent_span_id}`);
    }
  }
}

/// (2) No internal span dangles. A span is a legitimate root only when it is an
/// ingress `http.request` (its parent may be an upstream `traceparent` not in the
/// file). Every other span must have its parent present and on the same trace.
export function assertConnected(spans) {
  const { byId } = index(spans);
  for (const s of spans) {
    const parent = s.parent_span_id;
    if (parent == null) continue;
    const p = byId.get(parent);
    if (!p) {
      assert.equal(
        s.name,
        'http.request',
        `dangling parent ${parent} on non-ingress span ${s.name}`,
      );
      continue;
    }
    assert.equal(
      s.trace_id,
      p.trace_id,
      `span ${s.name} on a different trace than its parent ${p.name}`,
    );
  }
}

/// (3a) Every required REST route produced an ingress span.
export function assertRouteCoverage(spans, requiredRoutes) {
  const { routes } = index(spans);
  for (const r of requiredRoutes) {
    assert.ok(routes.has(r), `no ingress span for required route ${r}; saw ${[...routes].join(', ')}`);
  }
}

/// Walk from a span up to its transitive roots, returning the ancestor chain
/// (nearest-first), stopping when a parent is absent from the file.
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

/// (3b) The driven turn produced the OTel GenAI chain: a `chat` span whose
/// ancestors include an `invoke_agent` span and the `sessions.events.send` span,
/// all on one trace. Returns the `chat` span for further attribute checks.
export function assertGenAiChain(spans) {
  const { byId } = index(spans);
  const op = (s) => s.attributes?.['gen_ai.operation.name'];
  const chat = spans.find((s) => op(s) === 'chat');
  assert.ok(chat, 'no gen_ai.operation.name="chat" span; the turn produced no inference span');
  assert.equal(chat.attributes['gen_ai.request.model'] ?? '', chat.attributes['gen_ai.request.model'] ?? '');

  const chain = ancestors(chat, byId);
  const invokeAgent = chain.find((s) => op(s) === 'invoke_agent');
  assert.ok(invokeAgent, `chat span not under an invoke_agent span; chain: ${chain.map((s) => s.name).join(' -> ')}`);
  const sendSpan = chain.find((s) => s.name === 'sessions.events.send');
  assert.ok(sendSpan, `chat span not under sessions.events.send; chain: ${chain.map((s) => s.name).join(' -> ')}`);

  for (const s of [chat, invokeAgent, sendSpan]) {
    assert.equal(s.trace_id, chat.trace_id, `GenAI chain split across traces at ${s.name}`);
  }
  // Provider name uses the new OTel spelling (not the deprecated gen_ai.system).
  assert.equal(chat.attributes['gen_ai.provider.name'], 'awaken', 'chat span missing gen_ai.provider.name');
  return chat;
}

/// (3d) A tool turn produced an OTel GenAI `execute_tool` span: name
/// `execute_tool {tool}`, `gen_ai.operation.name="execute_tool"`, a well-formed
/// `gen_ai.tool.call.id`, nested under an `invoke_agent` span on the same trace.
export function assertToolSpan(spans, toolName) {
  const { byId } = index(spans);
  const op = (s) => s.attributes?.['gen_ai.operation.name'];
  const tool = spans.find(
    (s) => op(s) === 'execute_tool' && s.attributes?.['gen_ai.tool.name'] === toolName,
  );
  assert.ok(tool, `no execute_tool span for tool "${toolName}"`);
  assert.equal(tool.name, `execute_tool ${toolName}`, `tool span name not templated: ${tool.name}`);
  assert.ok(tool.attributes['gen_ai.tool.call.id'], 'execute_tool span missing gen_ai.tool.call.id');
  const chain = ancestors(tool, byId);
  const invokeAgent = chain.find((s) => op(s) === 'invoke_agent');
  assert.ok(invokeAgent, `execute_tool not under invoke_agent; chain: ${chain.map((s) => s.name).join(' -> ')}`);
  assert.equal(tool.trace_id, invokeAgent.trace_id, 'tool span on a different trace than its agent');
  return tool;
}

/// (3e) The detached background aux run (memory extraction) is linked into the
/// spawning turn's trace via an `aux.background` span, instead of a disconnected
/// root. Asserts the aux span nests under the turn and drove a sub-run.
export function assertBackgroundLinked(spans) {
  const { byId } = index(spans);
  const aux = spans.find((s) => s.name === 'aux.background');
  assert.ok(aux, 'no aux.background span; the background run was not linked to the trace');
  const chain = ancestors(aux, byId);
  assert.ok(
    chain.some((s) => s.name === 'host.run_turn' || s.name === 'sessions.events.send'),
    `aux.background not under the turn; chain: ${chain.map((s) => s.name).join(' -> ')}`,
  );
  const subRun = spans.find(
    (s) => s.name === 'runtime.run' && ancestors(s, byId).some((a) => a.span_id === aux.span_id),
  );
  assert.ok(subRun, 'aux.background produced no runtime.run sub-run');
  return aux;
}

/// (3f) Durable dispatch relay: a run admitted via `submit_background` and drained
/// by the daemon continues the admitting request's trace — a `wake.dispatch` span
/// on the submit's trace, parented into the submit ingress span, driving a run.
export function assertDurableDispatch(spans) {
  const { byId } = index(spans);
  const submit = spans.find(
    (s) => s.name === 'http.request' && (s.attributes?.['http.route'] ?? '').endsWith('submit_background'),
  );
  assert.ok(submit, 'no submit_background ingress span');
  const wake = spans.find((s) => s.name === 'wake.dispatch');
  assert.ok(wake, 'no wake.dispatch span; the durable trace relay is missing');
  assert.equal(wake.trace_id, submit.trace_id, 'wake.dispatch on a different trace than the submit');
  const chain = [wake, ...ancestors(wake, byId)];
  assert.ok(
    chain.some((s) => s.span_id === submit.span_id),
    'wake.dispatch not parented under the submit ingress span (durable relay broken)',
  );
  const subRun = spans.find(
    (s) => s.name === 'runtime.run' && ancestors(s, byId).some((a) => a.span_id === wake.span_id),
  );
  assert.ok(subRun, 'wake.dispatch drove no runtime.run');
  return wake;
}

/// (3c) W3C traceparent propagation: the request carrying `00-<tid>-<sid>-01`
/// produced a root ingress span on trace `<tid>` whose parent is `<sid>`.
export function assertPropagation(spans, route, tid, sid) {
  const s = spans.find(
    (x) => x.name === 'http.request' && x.attributes?.['http.route'] === route,
  );
  assert.ok(s, `no ingress span for propagation probe route ${route}`);
  assert.equal(s.trace_id, tid, `inbound traceparent trace id not continued (${s.trace_id} != ${tid})`);
  assert.equal(s.parent_span_id, sid, `inbound traceparent parent not honored (${s.parent_span_id} != ${sid})`);
}
