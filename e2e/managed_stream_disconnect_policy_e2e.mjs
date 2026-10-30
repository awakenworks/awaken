// T32-C01 (断流策略) through the real production `awaken` process and real
// sockets. A Native Run is held in flight at a deterministic upstream gate (a
// promise, not a timer). While it is active the client hard-closes both of its
// HTTP connections to the service: the Managed event SSE stream and the still
// pending `POST /v1/sessions/{id}/events` request. The Run must then continue
// under its own durable owner; an explicit cancel is a separate command with
// its own receipt.
//
// Oracles (groups-5 T32-C01, 0-based):
//   O0 the deployment's selected policy for a production Managed Session
//      (owner-scoped => detach) is applied: the disconnect neither cancels the
//      in-flight inference nor writes a terminal; the Run ends with end_turn;
//   O1 the connection owner does not take the Run owner: closing the
//      connection creates no Run-level event, no second inference and no
//      interrupt; a fresh, unrelated connection observes the same terminal;
//   O2 an explicit cancel (`user.interrupt`) has its own receipt: a distinct
//      id, committed exactly once with processed_at, followed by its outcome.
//
// Negative control (executed): the same `detachDefects` / `cancelDefects`
// checkers are run on mutants of the real history in which the socket close
// was mapped to a Run terminal, or the cancel shares the message's receipt;
// each must be reported.
//
// Run: build the production binary first (harness ensureProductionBuilt, or
// AWAKEN_BIN), then
//   E2E_PORT=<free port> node e2e/managed_stream_disconnect_policy_e2e.mjs
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import Anthropic from '@anthropic-ai/sdk';
import {
  spawnProduction, stopServer, waitForPort, waitForSessionEventReceipt, waitForValue,
  listManagedSessionEvents, provisionProviderTestModel, cleanupFixtureTree, pass,
} from './harness.mjs';
import { withoutAmbientProviderKeys } from './process_environment.mjs';
import { startFakeAnthropic } from './fixtures/fake_anthropic_fixture.mjs';

const BETAS = ['managed-agents-2026-04-01'];
const PORT = Number(process.env.E2E_PORT ?? 38632);
const HEADERS = {
  'x-api-key': 'e2e-local', // awaken-allow(secret): (local no-login placeholder)
  'anthropic-version': '2023-06-01',
  'anthropic-beta': BETAS.join(','),
};
const MARK = 'T32C01-turn-';
const replyFor = (turn) => `reply-${turn}`;

const directory = fs.mkdtempSync(path.join(os.tmpdir(), 'awaken-t32c01-'));
const data = path.join(directory, 'server-data');
const base = `http://127.0.0.1:${PORT}`;
let upstream;
let server;

// ---- deterministic, gated model ---------------------------------------------
function lastUserText(request) {
  const users = (request.messages ?? []).filter((m) => m.role === 'user');
  const content = users.at(-1)?.content ?? '';
  return typeof content === 'string' ? content
    : content.map((block) => (block.type === 'text' ? block.text : '')).join('');
}
const gates = new Map();
function gateFor(turn) {
  if (!gates.has(turn)) {
    const gate = { calls: 0, signals: [] };
    gate.arrived = new Promise((resolve) => { gate.arrive = resolve; });
    gate.released = new Promise((resolve) => { gate.release = resolve; });
    gates.set(turn, gate);
  }
  return gates.get(turn);
}
async function behavior(request, _ordinal, signal) {
  const turn = new RegExp(`${MARK}(\\w+)`, 'u').exec(lastUserText(request))?.[1];
  if (!turn) return { text: 'auxiliary' };
  const gate = gateFor(turn);
  gate.calls += 1;
  gate.signals.push(signal);
  gate.arrive();
  await gate.released;
  return { text: replyFor(turn) };
}

// Fail loudly instead of hanging when a causal promise never settles.
function withDeadline(promise, ms, label) {
  let timer;
  const expired = new Promise((_, reject) => { timer = setTimeout(() => reject(new Error(`${label}: not settled in ${ms}ms`)), ms); });
  return Promise.race([promise, expired]).finally(() => clearTimeout(timer));
}

// ---- the oracle checkers (shared by the real run and the negative controls) --
const turnSegment = (history, turn) => {
  const start = history.findIndex((e) => e.type === 'user.message'
    && JSON.stringify(e.content).includes(`${MARK}${turn}`));
  if (start < 0) return null;
  const next = history.findIndex((e, i) => i > start && e.type === 'user.message');
  return { start, events: history.slice(start, next < 0 ? history.length : next) };
};

// O0/O1 for the disconnected turn.
export function detachDefects(history, turn, { upstreamCalls, upstreamAborted }) {
  const defects = [];
  const segment = turnSegment(history, turn);
  if (!segment) return [`turn ${turn}: user.message not committed`];
  const { events } = segment;
  const messages = events.filter((e) => e.type === 'agent.message');
  const idles = events.filter((e) => e.type === 'session.status_idle');
  const errors = events.filter((e) => e.type === 'session.error');
  const interrupts = events.filter((e) => e.type === 'user.interrupt');
  if (interrupts.length !== 0) defects.push(`O1 the disconnect produced ${interrupts.length} interrupt(s)`);
  if (errors.length !== 0) defects.push(`O0 the disconnect produced session.error ${JSON.stringify(errors.map((e) => e.error?.type))}`);
  if (idles.length !== 1 || idles[0].stop_reason?.type !== 'end_turn') {
    defects.push(`O0 terminals ${JSON.stringify(idles.map((e) => e.stop_reason?.type))}, expected one end_turn`);
  }
  const text = messages.map((m) => m.content.map((b) => b.text ?? '').join('')).join('|');
  if (messages.length !== 1 || text !== replyFor(turn)) defects.push(`O0 the Run's reply was not committed once (${messages.length}, ${text})`);
  const reply = events.findIndex((e) => e.type === 'agent.message');
  const idle = events.findIndex((e) => e.type === 'session.status_idle');
  if (reply < 0 || idle < reply) defects.push('O0 the terminal precedes the Run reply');
  if (upstreamCalls !== 1) defects.push(`O1 ${upstreamCalls} inference(s) for one Run`);
  if (upstreamAborted) defects.push('O0 the disconnect cancelled the in-flight inference');
  return defects;
}

// O2 for the explicitly cancelled turn.
export function cancelDefects(history, messageReceipt, interruptReceipt) {
  const defects = [];
  if (interruptReceipt?.type !== 'user.interrupt') defects.push(`O2 cancel receipt type ${interruptReceipt?.type}`);
  if (!interruptReceipt?.id || interruptReceipt.id === messageReceipt?.id) defects.push('O2 the cancel has no receipt of its own');
  const committed = history.filter((e) => e.id === interruptReceipt?.id);
  if (committed.length !== 1) defects.push(`O2 cancel receipt committed ${committed.length} times`);
  else if (!committed[0].processed_at) defects.push('O2 cancel receipt has no processed_at');
  const at = history.findIndex((e) => e.id === interruptReceipt?.id);
  const outcome = history.slice(at + 1).find((e) => e.type === 'session.status_idle' || e.type === 'session.error');
  if (at < 0 || !outcome) defects.push('O2 the cancel has no committed outcome');
  return defects;
}

// ---- one SSE reader that can be hard-closed --------------------------------
async function openStream(sessionId, controller) {
  const response = await fetch(`${base}/v1/sessions/${sessionId}/events/stream`, {
    headers: HEADERS, signal: controller.signal,
  });
  assert.equal(response.status, 200, 'event stream accepted');
  const reader = response.body.getReader();
  const decoder = new TextDecoder();
  let buffer = '';
  // Read until the stream has delivered this turn's committed user.message,
  // proving the connection is live and attached to the active Session.
  for (;;) {
    const { value, done } = await reader.read();
    if (done) throw new Error('event stream ended before the active turn was observed');
    buffer += decoder.decode(value, { stream: true });
    if (buffer.includes(MARK)) return reader;
  }
}

async function main() {
  assert.ok(Number.isInteger(PORT) && PORT > 0, 'E2E_PORT is required');
  const key = 't32c01-fixture-key'; // awaken-allow(secret): (synthetic upstream)
  const model = 't32c01-fixture';
  upstream = await startFakeAnthropic(key, { models: [model], behavior });
  server = spawnProduction(data, PORT, { fields: { log_filter: 'warn' } }, withoutAmbientProviderKeys(process.env));
  await waitForPort(PORT, 60_000, server);
  const context = await fetch(`${base}/v1/config/workspace-context`);
  assert.ok(context.ok, 'workspace context');
  const workspace = context.headers.get('anthropic-workspace-id');
  const published = await provisionProviderTestModel(base, workspace, {
    id: 't32c01', provider_id: 'anthropic', dialect: 'anthropic_messages', base_url: `${upstream.url}/v1`,
    secret: key, model_id: model, timeout_secs: 120,
  });
  const client = new Anthropic({ apiKey: 'e2e-local', baseURL: base, maxRetries: 0 }); // awaken-allow(secret): (local no-login placeholder)
  const agent = await client.beta.agents.create({ name: 'Disconnect policy', model: published.model, system: 'Reply with the configured text.', betas: BETAS });
  const create = () => client.beta.sessions.create({ agent: { type: 'agent', id: agent.id, version: agent.version }, environment_id: 'env_local', betas: BETAS });

  // ---- D: hard-close every client connection while the Run is in flight ----
  const detached = await create();
  const gateD = gateFor('D');
  const sendAbort = new AbortController();
  const sending = fetch(`${base}/v1/sessions/${detached.id}/events`, {
    method: 'POST',
    headers: { ...HEADERS, 'content-type': 'application/json' },
    body: JSON.stringify({ events: [{ type: 'user.message', content: [{ type: 'text', text: `${MARK}D reply briefly.` }] }] }),
    signal: sendAbort.signal,
  }).then((response) => ({ settled: true, status: response.status }), (error) => ({ settled: false, error: error.name }));
  await withDeadline(gateD.arrived, 120_000, 'D: inference arrival');
  const streamAbort = new AbortController();
  const reader = await withDeadline(openStream(detached.id, streamAbort), 60_000, 'D: stream attach');
  // The disconnect: both sockets this client holds to the service close now.
  streamAbort.abort();
  sendAbort.abort();
  await reader.cancel().catch(() => {});
  const sendOutcome = await sending;
  // Still in flight at the gate: the disconnect wrote no terminal and did not
  // cancel the upstream inference. This is read before the gate opens, so it
  // is a durable observation, not a race.
  const whileHeld = await listManagedSessionEvents(client, detached.id, BETAS);
  const heldSegment = turnSegment(whileHeld, 'D');
  assert.ok(heldSegment, 'D: the user.message was committed before the disconnect');
  assert.deepEqual(
    heldSegment.events.filter((e) => ['session.status_idle', 'session.error', 'user.interrupt'].includes(e.type)).map((e) => e.type),
    [],
    'O0/O1: no terminal, error or interrupt was written by the disconnect',
  );
  assert.equal(gateD.signals[0]?.aborted ?? false, false, 'O0: the in-flight inference survived the disconnect');
  gateD.release();
  // A fresh connection (a new SDK client) observes the original Run's terminal.
  const observer = new Anthropic({ apiKey: 'e2e-local', baseURL: base, maxRetries: 0 }); // awaken-allow(secret): (local no-login placeholder)
  const settled = await waitForValue(
    () => listManagedSessionEvents(observer, detached.id, BETAS),
    (history) => turnSegment(history, 'D')?.events.some((e) => e.type === 'session.status_idle' || e.type === 'session.error'),
    'D: the detached Run reaches its committed terminal',
    { timeoutMs: 120_000, pollMs: 100 },
  );
  const facts = { upstreamCalls: gateD.calls, upstreamAborted: gateD.signals.some((s) => s?.aborted) };
  assert.deepEqual(detachDefects(settled, 'D', facts), [], 'T32-C01 O0/O1 detach policy on the production service');
  const retrieved = await observer.beta.sessions.retrieve(detached.id, { betas: BETAS });
  assert.equal(retrieved.status, 'idle', 'O1: the Session settled under its own owner');
  pass(`T32-C01 detach: Run ended end_turn after both sockets closed (send ${JSON.stringify(sendOutcome)})`);

  // ---- X: an explicit cancel is its own command with its own receipt ------
  const cancelled = await create();
  const gateX = gateFor('X');
  const sendingX = client.beta.sessions.events.send(cancelled.id, {
    events: [{ type: 'user.message', content: [{ type: 'text', text: `${MARK}X reply briefly.` }] }], betas: BETAS,
  });
  sendingX.catch(() => {});
  await withDeadline(gateX.arrived, 120_000, 'X: inference arrival');
  const messageReceipt = (await withDeadline(sendingX, 120_000, 'X: message receipt')).data[0];
  const interruptReceipt = (await client.beta.sessions.events.send(cancelled.id, {
    events: [{ type: 'user.interrupt' }], betas: BETAS,
  })).data[0];
  const { events: afterCancel } = await waitForSessionEventReceipt(client, cancelled.id, interruptReceipt.id, BETAS,
    ({ delta }) => delta.some((e) => e.type === 'session.status_idle' || e.type === 'session.error'),
    'X: the interrupt receipt and its outcome are committed', { timeoutMs: 120_000 });
  gateX.release();
  assert.deepEqual(cancelDefects(afterCancel, messageReceipt, interruptReceipt), [], 'T32-C01 O2 explicit cancel receipt');
  pass(`T32-C01 cancel: interrupt ${interruptReceipt.id} is distinct from message ${messageReceipt.id}`);

  // ---- negative controls on the real histories -----------------------------
  const segment = turnSegment(settled, 'D');
  const closeIsTerminal = [
    ...settled.slice(0, segment.start + 1),
    { type: 'session.status_idle', id: 'sevt_nc_close', stop_reason: { type: 'interrupted' } },
  ];
  assert.ok(detachDefects(closeIsTerminal, 'D', facts).some((d) => d.startsWith('O0')), 'NC: close->terminal is reported');
  assert.ok(detachDefects(settled, 'D', { upstreamCalls: 1, upstreamAborted: true }).some((d) => d.startsWith('O0')), 'NC: close->cancelled inference is reported');
  assert.ok(detachDefects(settled, 'D', { upstreamCalls: 2, upstreamAborted: false }).some((d) => d.startsWith('O1')), 'NC: a reconnect-started second Run is reported');
  const injectedInterrupt = [...settled.slice(0, segment.start + 1), { type: 'user.interrupt', id: 'sevt_nc_int' }, ...settled.slice(segment.start + 1)];
  assert.ok(detachDefects(injectedInterrupt, 'D', facts).some((d) => d.startsWith('O1')), 'NC: close->interrupt is reported');
  assert.ok(cancelDefects(afterCancel, messageReceipt, { ...messageReceipt }).some((d) => d.startsWith('O2')), 'NC: a cancel without its own receipt is reported');
  pass('T32-C01 negative controls: socket close mapped to a terminal/cancel and a shared receipt are detected');
}

try {
  await main();
  console.log('\nE2E PASS: T32-C01 disconnect detaches; explicit cancel has its own receipt');
} catch (error) {
  console.error(`E2E FAIL: ${error.stack}`);
  process.exitCode = 1;
} finally {
  for (const gate of gates.values()) gate.release();
  if (server) await stopServer(server);
  if (upstream) await upstream.close();
  cleanupFixtureTree(directory);
}
