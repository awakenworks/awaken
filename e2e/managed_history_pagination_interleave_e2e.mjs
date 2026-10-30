// T32-C03 "paging interleaved with previews": page one Managed Session's
// durable event history with a tiny page size while live previews, Run
// terminals and a real production-process restart are appended/interleaved.
//
// Tier L3: the real `awaken` production binary (spawnProduction), the real
// HTTP wire (fetch for `GET /v1/sessions/{id}/events?limit=&page=` and the
// SSE preview stream, official @anthropic-ai/sdk for authoring/sends) and the
// real Native provider path against the deterministic fake Anthropic upstream.
// The fake holds each turn's inference on an in-test gate (a promise, not a
// timer), so "a page is read while the Run is in flight" and "a page is read
// after the preview started" are causal, not scheduled.
//
// Cause/effect graph. C1=a page is read while a Run is in flight; C2=a page is
// read after the opt-in preview (`event_start`) for an agent.message arrived;
// C3=the Run terminal commits between pages; C4=the production process is
// stopped and relaunched on the same data dir while the client holds a cursor
// in the middle of history; C5=the client presents a cursor that is not on the
// durable prefix. Effects: E1=the concatenated pages equal the final durable
// list exactly (ids, order, types; no gap, duplicate or regression);
// E2=preview frames never appear in pages and the preview id resolves to one
// committed agent.message; E3=each turn has exactly one agent.message and one
// end_turn terminal; E4=the saved cursor is accepted after restart and the next
// page starts at the cursor's durable successor; the pre-restart durable prefix
// is unchanged after restart; E5=C5 is a 400, never a silent restart at 0.
// Decision table: P1(C1)->E1; P2(C2)->E1+E2; P3(C3)->E1+E3; P4(C4)->E1+E4;
// P5(C5)->E5. Oracles (groups-5 T32-C03, 0-based): O0 = E1+E2+E3, O1 = E4+E5.
//
// Negative control (executed, not described): the same `historyDefects` /
// `terminalDefects` checkers are run against in-test mutants that keep the
// event COUNT unchanged (drop+duplicate, adjacent swap, a preview frame in place
// of a durable event, a second terminal in place of a message) and against the
// "disconnect without recovery" mutant (restart pagination from zero after the
// restart). Each must be detected while a count-only check accepts the
// count-preserving mutants. The run itself contains a real process restart
// (distinct child pids) and resumes from the pre-restart cursor.
//
// Run: build the production binary first (see e2e/cargo_binary.mjs; the harness
// builds `awaken-cli --bin awaken` on demand or uses AWAKEN_BIN), then
//   E2E_PORT=<free port> node e2e/managed_history_pagination_interleave_e2e.mjs
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import Anthropic from '@anthropic-ai/sdk';
import {
  spawnProduction, stopServer, waitForPort, waitForSessionEventReceipt, listManagedSessionEvents,
  provisionProviderTestModel, cleanupFixtureTree, pass,
} from './harness.mjs';
import { withoutAmbientProviderKeys } from './process_environment.mjs';
import { startFakeAnthropic } from './fixtures/fake_anthropic_fixture.mjs';

const BETAS = ['managed-agents-2026-04-01'];
const PORT = Number(process.env.E2E_PORT ?? 38633);
const PAGE_LIMIT = 2;
const PREVIEW_TYPES = new Set(['event_start', 'event_delta']);
const HEADERS = {
  'x-api-key': 'e2e-local', // awaken-allow(secret): (local no-login placeholder)
  'anthropic-version': '2023-06-01',
  'anthropic-beta': BETAS.join(','),
};
const MARK = 'T32C03-turn-';
const replyFor = (turn) => `reply-${turn} ${'durable-preview-chunk '.repeat(60)}`.trim();

const directory = fs.mkdtempSync(path.join(os.tmpdir(), 'awaken-t32c03-'));
const data = path.join(directory, 'server-data');
const base = `http://127.0.0.1:${PORT}`;
let upstream;
let server;
const pids = [];

const launch = async () => {
  server = spawnProduction(data, PORT, { fields: { log_filter: 'warn' } },
    withoutAmbientProviderKeys(process.env));
  await waitForPort(PORT, 60_000, server);
  pids.push(server.pid);
};

// ---- deterministic, gated model ------------------------------------------------
function lastUserText(request) {
  const users = (request.messages ?? []).filter((m) => m.role === 'user');
  const content = users.at(-1)?.content ?? '';
  return typeof content === 'string' ? content
    : content.map((block) => (block.type === 'text' ? block.text : '')).join('');
}
const gates = new Map();
function gateFor(turn) {
  if (!gates.has(turn)) {
    const gate = {};
    gate.arrived = new Promise((resolve) => { gate.arrive = resolve; });
    gate.released = new Promise((resolve) => { gate.release = resolve; });
    gates.set(turn, gate);
  }
  return gates.get(turn);
}
async function behavior(request) {
  const turn = new RegExp(`${MARK}(\\w+)`, 'u').exec(lastUserText(request))?.[1];
  if (!turn) return { text: 'auxiliary' };
  const gate = gateFor(turn);
  gate.arrive();
  await gate.released;
  return { text: replyFor(turn) };
}

// ---- one paging client over the real HTTP list API -----------------------------
async function listPage(sessionId, cursor) {
  const query = new URLSearchParams({ limit: String(PAGE_LIMIT) });
  if (cursor) query.set('page', cursor);
  const response = await fetch(`${base}/v1/sessions/${sessionId}/events?${query}`, { headers: HEADERS });
  return { status: response.status, body: await response.json() };
}
function pager(sessionId) {
  const state = { events: [], steps: [], cursor: null };
  // One page; the continuation is the server's next_page, or (at the live end
  // of history, next_page=null) the id of the last delivered event, which is
  // the documented after-id cursor of this list (state/events/stream.rs).
  state.step = async (label) => {
    const before = state.cursor;
    const { status, body } = await listPage(sessionId, before);
    assert.equal(status, 200, `${label}: page after ${before} accepted (${JSON.stringify(body)})`);
    assert.ok(body.data.length <= PAGE_LIMIT, `${label}: page honours limit=${PAGE_LIMIT}`);
    if (body.next_page) assert.equal(body.next_page, body.data.at(-1).id, `${label}: next_page continues after the last row`);
    state.events.push(...body.data);
    state.cursor = body.next_page ?? body.data.at(-1)?.id ?? before;
    state.steps.push({ label, before, after: state.cursor, ids: body.data.map((e) => e.id), more: body.next_page !== null });
    return body;
  };
  state.drain = async (label) => {
    for (let index = 0; index < 500; index += 1) {
      if (!(await state.step(`${label}#${index}`)).next_page) return;
    }
    assert.fail(`${label}: history did not end within 500 pages`);
  };
  return state;
}

// ---- the oracle checkers (shared by the real run and the negative controls) ----
// O0/O1: the concatenated pages must be exactly the durable list.
function historyDefects(paged, durable) {
  const defects = [];
  const seen = new Set();
  paged.forEach((event, index) => {
    if (PREVIEW_TYPES.has(event.type)) defects.push(`preview frame ${event.type} at page position ${index}`);
    if (seen.has(event.id)) defects.push(`duplicate ${event.id} at page position ${index}`);
    seen.add(event.id);
  });
  for (let index = 0; index < Math.max(paged.length, durable.length); index += 1) {
    const page = paged[index];
    const truth = durable[index];
    if (page?.id !== truth?.id) {
      defects.push(`position ${index}: paged ${page?.id ?? 'none'} vs durable ${truth?.id ?? 'none'}`);
    } else if (page.type !== truth.type) {
      defects.push(`position ${index}: ${page.id} type ${page.type} vs durable ${truth.type}`);
    } else if (page.processed_at && !truth.processed_at) {
      defects.push(`position ${index}: ${page.id} processed_at regressed`);
    }
  }
  return defects;
}
const countOnly = (paged, durable) => paged.length === durable.length;

// O0 "wrong terminal": every turn (segment after its user.message) carries one
// agent.message with the turn's exact reply and then one end_turn idle.
function terminalDefects(durable, turns) {
  const defects = [];
  const starts = durable.flatMap((event, index) => (event.type === 'user.message' ? [index] : []));
  if (starts.length !== turns.length) defects.push(`expected ${turns.length} user turns, found ${starts.length}`);
  starts.forEach((start, turnIndex) => {
    const segment = durable.slice(start + 1, starts[turnIndex + 1] ?? durable.length);
    const messages = segment.filter((e) => e.type === 'agent.message');
    const idles = segment.filter((e) => e.type === 'session.status_idle');
    const text = messages.map((m) => m.content.map((b) => b.text ?? '').join('')).join('|');
    if (messages.length !== 1 || text !== replyFor(turns[turnIndex])) defects.push(`turn ${turns[turnIndex]}: messages ${messages.length} text ok=${text === replyFor(turns[turnIndex])}`);
    if (idles.length !== 1 || idles[0].stop_reason?.type !== 'end_turn') defects.push(`turn ${turns[turnIndex]}: terminals ${JSON.stringify(idles.map((e) => e.stop_reason?.type))}`);
    if (segment.at(-1)?.type !== 'session.status_idle') defects.push(`turn ${turns[turnIndex]}: terminal is not the last event (${segment.at(-1)?.type})`);
  });
  return defects;
}

// ---- one turn: send, page while held, preview, page on preview, terminal ------
// Fail loudly instead of hanging when a causal promise never settles.
function withDeadline(promise, ms, label) {
  let timer;
  const expired = new Promise((_, reject) => { timer = setTimeout(() => reject(new Error(`${label}: not settled in ${ms}ms`)), ms); });
  return Promise.race([promise, expired]).finally(() => clearTimeout(timer));
}

async function turn(client, sessionId, pages, name) {
  const gate = gateFor(name);
  const durableBefore = await listManagedSessionEvents(client, sessionId, BETAS);
  const idlesBefore = new Set(durableBefore.filter((e) => e.type === 'session.status_idle').map((e) => e.id));
  // The send may hold its response until the Run commits, so it is not awaited
  // before the gated inference is released (no self-deadlock on the gate).
  const sending = client.beta.sessions.events.send(sessionId, {
    events: [{ type: 'user.message', content: [{ type: 'text', text: `${MARK}${name} reply briefly.` }] }], betas: BETAS,
  });
  sending.catch(() => {});
  await withDeadline(Promise.race([gate.arrived, sending.then(() => gate.arrived)]), 120_000, `${name}: inference arrival`);
  // C1: inference is provably in flight (held at the fake) while we page.
  await pages.step(`${name}:in-flight`);
  const stream = await fetch(`${base}/v1/sessions/${sessionId}/events/stream?event_deltas[]=agent.message`, {
    headers: HEADERS, signal: AbortSignal.timeout(120_000),
  });
  assert.equal(stream.status, 200, `${name}: preview stream accepted`);
  gate.release();
  const frames = [];
  const reader = stream.body.getReader();
  const decoder = new TextDecoder();
  let buffer = '';
  let pagedDuringPreview = false;
  let finished = false;
  for (;;) {
    const { value, done } = await reader.read();
    if (done) break;
    buffer += decoder.decode(value, { stream: true });
    const blocks = buffer.split('\n\n');
    buffer = blocks.pop();
    for (const line of blocks.flatMap((block) => block.split('\n'))) {
      if (!line.startsWith('data: ')) continue;
      const frame = JSON.parse(line.slice('data: '.length));
      frames.push(frame);
      // C2: a page is read after this Run's preview began.
      if (frame.type === 'event_start' && !pagedDuringPreview) {
        pagedDuringPreview = true;
        await pages.step(`${name}:after-preview-start`);
      }
      if (frame.type === 'session.status_idle' && !idlesBefore.has(frame.id)) finished = true;
    }
    if (finished) break;
  }
  await reader.cancel().catch(() => {});
  assert.ok(finished, `${name}: stream delivered this Run's terminal; frames ${JSON.stringify(frames.map((f) => f.type))}`);
  const start = frames.find((f) => f.type === 'event_start' && f.event?.type === 'agent.message');
  assert.ok(start && pagedDuringPreview, `${name}: a live preview was observed and a page was read after it`);
  const previewText = frames.filter((f) => f.type === 'event_delta' && f.event_id === start.event.id)
    .map((f) => f.delta.content.text).join('');
  assert.equal(previewText, replyFor(name), `${name}: preview deltas carry the reply`);
  const receipt = (await withDeadline(sending, 120_000, `${name}: send receipt`)).data[0];
  const { events } = await waitForSessionEventReceipt(client, sessionId, receipt.id, BETAS,
    ({ delta }) => delta.some((e) => e.type === 'agent.message') && delta.some((e) => e.type === 'session.status_idle'),
    `${name}: Run terminal committed after its exact receipt`, { timeoutMs: 120_000 });
  const committed = events.filter((e) => e.id === start.event.id);
  assert.equal(committed.length, 1, `${name}: preview id resolves to exactly one durable event`);
  assert.equal(committed[0].type, 'agent.message', `${name}: the preview id is the committed agent.message`);
  return { receipt, previewId: start.event.id };
}

async function main() {
  assert.ok(Number.isInteger(PORT) && PORT > 0, 'E2E_PORT is required');
  const key = 't32c03-fixture-key'; // awaken-allow(secret): (synthetic upstream)
  const model = 't32c03-fixture';
  upstream = await startFakeAnthropic(key, { models: [model], behavior });
  await launch();
  const context = await fetch(`${base}/v1/config/workspace-context`);
  assert.ok(context.ok, 'workspace context');
  const workspace = context.headers.get('anthropic-workspace-id');
  const published = await provisionProviderTestModel(base, workspace, {
    id: 't32c03', provider_id: 'anthropic', dialect: 'anthropic_messages', base_url: `${upstream.url}/v1`,
    secret: key, model_id: model, timeout_secs: 120,
  });
  let client = new Anthropic({ apiKey: 'e2e-local', baseURL: base, maxRetries: 0 }); // awaken-allow(secret): (local no-login placeholder)
  const agent = await client.beta.agents.create({ name: 'Paging interleave', model: published.model, system: 'Reply with the configured text.', betas: BETAS });
  const session = await client.beta.sessions.create({ agent: { type: 'agent', id: agent.id, version: agent.version }, environment_id: 'env_local', betas: BETAS });
  const pages = pager(session.id);

  // Phase A: turn A with paging in flight and during its preview.
  const first = await turn(client, session.id, pages, 'A');
  const beforeRestart = await listManagedSessionEvents(client, session.id, BETAS);
  const savedCursor = pages.cursor;
  const savedIndex = beforeRestart.findIndex((e) => e.id === savedCursor);
  assert.ok(savedIndex >= 0 && savedIndex < beforeRestart.length - 1,
    `the saved cursor (${savedCursor}, index ${savedIndex}) is mid-history of ${beforeRestart.length} events`);
  assert.deepEqual(historyDefects(pages.events, beforeRestart.slice(0, pages.events.length)), [], 'pre-restart pages are the durable prefix');

  // Phase B: real restart (same data dir) while the client holds the cursor.
  await stopServer(server);
  server = undefined;
  await launch();
  assert.notEqual(pids[0], pids[1], 'a new production process serves the resumed pagination');
  client = new Anthropic({ apiKey: 'e2e-local', baseURL: base, maxRetries: 0 }); // awaken-allow(secret): (local no-login placeholder)
  const afterRestart = await listManagedSessionEvents(client, session.id, BETAS);
  assert.ok(afterRestart.length >= beforeRestart.length, 'O1: no durable event is lost across restart');
  assert.deepEqual(historyDefects(afterRestart.slice(0, beforeRestart.length), beforeRestart), [],
    'O1: the durable prefix is unchanged across restart');
  // C5 (executed against the real service): a cursor not on the durable prefix
  // is refused instead of silently restarting at 0.
  const forged = await listPage(session.id, 'sevt_t32c03_not_on_prefix');
  assert.equal(forged.status, 400, `O1: unknown cursor is a 400 (${JSON.stringify(forged.body)})`);
  const resumed = await pages.step('B:resume-after-restart');
  assert.equal(pages.steps.at(-1).before, savedCursor, 'O1: pagination resumed from the pre-restart cursor');
  assert.equal(resumed.data[0]?.id, afterRestart[savedIndex + 1]?.id, 'O1: the first resumed row is the cursor\'s durable successor');

  // Phase C: turn B after restart, again paged in flight and during preview.
  const second = await turn(client, session.id, pages, 'B');
  await pages.drain('C:drain');
  const durable = await listManagedSessionEvents(client, session.id, BETAS);
  // One more page from the held cursor absorbs anything committed between the
  // drain and the durable read; the comparison below is then exact.
  await pages.step('C:after-final');

  // O0 + O1 on the real run.
  assert.deepEqual(historyDefects(pages.events, durable), [], 'O0: pages == durable list (no gap/dup/regression)');
  assert.deepEqual(terminalDefects(durable, ['A', 'B']), [], 'O0: one committed message and one end_turn terminal per turn');
  for (const { previewId } of [first, second]) {
    assert.equal(durable.filter((e) => e.id === previewId).length, 1, 'O0: preview id committed once');
  }
  assert.ok(durable.every((e) => !PREVIEW_TYPES.has(e.type)), 'O0: preview frames never enter durable history');
  pages.steps.forEach((step, index) => {
    if (index > 0) assert.equal(step.before, pages.steps[index - 1].after, `O1: step ${step.label} continues the previous cursor`);
    if (step.ids.length > 0) {
      const at = durable.findIndex((e) => e.id === step.ids[0]);
      const expected = step.before === null ? 0 : durable.findIndex((e) => e.id === step.before) + 1;
      assert.equal(at, expected, `O1: step ${step.label} starts at the successor of its cursor`);
    }
  });
  const restartStep = pages.steps.findIndex((s) => s.label === 'B:resume-after-restart');
  assert.ok(pages.steps.slice(0, restartStep).some((s) => s.ids.length > 0) && pages.steps[restartStep].ids.length > 0,
    'pages were delivered both before and after the restart');
  pass(`T32-C03 real run: ${pages.steps.length} pages of ${PAGE_LIMIT} == ${durable.length} durable events across restart (pids ${pids.join('->')})`);

  // Negative controls: the same checkers detect count-preserving corruption
  // that a count-only check accepts, and a non-resumed (restart-from-0) client.
  const k = 1;
  const mutants = {
    'drop+duplicate': [...durable.slice(0, k), ...durable.slice(k + 1), durable.at(-1)],
    'adjacent swap': [...durable.slice(0, k), durable[k + 1], durable[k], ...durable.slice(k + 2)],
    'preview frame in place of durable event': durable.map((e, i) => (i === k ? { type: 'event_delta', id: e.id, event_id: e.id } : e)),
  };
  for (const [name, mutant] of Object.entries(mutants)) {
    assert.ok(countOnly(mutant, durable), `NC ${name}: count-only check is fooled`);
    assert.notDeepEqual(historyDefects(mutant, durable), [], `NC ${name}: history checker detects it`);
  }
  const notResumed = [...pages.events.slice(0, pages.events.findIndex((e) => e.id === savedCursor) + 1), ...durable];
  assert.notDeepEqual(historyDefects(notResumed, durable), [], 'NC restart-from-zero without cursor recovery is detected');
  const messageIndex = durable.findIndex((e) => e.type === 'agent.message');
  const lastIdle = durable.findLast((e) => e.type === 'session.status_idle');
  const secondTerminal = durable.map((e, i) => (i === messageIndex ? { ...lastIdle } : e));
  assert.ok(countOnly(secondTerminal, durable), 'NC wrong terminal keeps the count');
  assert.notDeepEqual(terminalDefects(secondTerminal, ['A', 'B']), [], 'NC wrong/second terminal is detected');
  const failedTerminal = durable.map((e) => (e.id === lastIdle.id ? { ...e, stop_reason: { type: 'retries_exhausted' } } : e));
  assert.notDeepEqual(terminalDefects(failedTerminal, ['A', 'B']), [], 'NC non-end_turn terminal is detected');
  pass('T32-C03 negative controls: count-preserving corruption and non-resumed paging are detected');
}

try {
  await main();
  console.log('\nE2E PASS: T32-C03 paging interleaved with previews, terminals and a restart');
} catch (error) {
  console.error(`E2E FAIL: ${error.stack}`);
  process.exitCode = 1;
} finally {
  for (const gate of gates.values()) gate.release();
  if (server) await stopServer(server);
  if (upstream) await upstream.close();
  cleanupFixtureTree(directory);
}
