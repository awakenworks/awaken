// T30-C03 (中断恢复行为) against the real production `awaken` process: the
// official TS SDK loses responses (a fetch wrapper performs the real request,
// then throws as if the reply never arrived), drops an event stream mid-Run, and
// the server process is hard-killed and respawned on the same data dir. The
// client then continues with the ORIGINAL Session id, Idempotency-Key ticket and
// event cursor.
//
// | Row | Fault | Continuation | Expected |
// |---|---|---|---|
// | R1 | lost response of sessions.create (keyed) | same key | the same Session; one Session exists |
// | R2 | lost response of events.send (keyed) | same key | the original receipt; one user.message; one inference |
// | R3 | event stream aborted while the Run is active | page from the last seen event id | the Run finished (detach); no gap, no duplicate |
// | R4 | hard kill + respawn | replay R1/R2 keys, page from a saved mid-history cursor | identical identities/receipts; same durable prefix; no new inference |
// | R5 | R1/R2 replays through raw HTTP instead of the SDK | - | the same ids/receipts (adapter parity) |
// | NC | the same create WITHOUT the original key | - | the Session-count checker reports the duplicate; the faults are proven injected |
//
// Oracles (groups-5): O0 public identity, the original receipt and the
// disconnect policy are kept; O1 completed effects are not restarted; O2 the
// recovery semantics are the same through each client adapter (SDK / raw HTTP).
//
// Run: build the production binary (harness ensureProductionBuilt / AWAKEN_BIN), then
//   E2E_PORT=<free port> node e2e/sdk_interruption_recovery_e2e.mjs
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import Anthropic from '@anthropic-ai/sdk';
import {
  spawnProduction, stopServer, hardKillServer, waitForPort, waitForSessionEventReceipt,
  listManagedSessionEvents, provisionProviderTestModel, cleanupFixtureTree, pass,
} from './harness.mjs';
import { withoutAmbientProviderKeys } from './process_environment.mjs';
import { startFakeAnthropic } from './fixtures/fake_anthropic_fixture.mjs';

const BETAS = ['managed-agents-2026-04-01'];
const PORT = Number(process.env.E2E_PORT ?? 38635);
const BASE = `http://127.0.0.1:${PORT}`;
const HEADERS = {
  'x-api-key': 'e2e-local', // awaken-allow(secret): (local no-login placeholder)
  'anthropic-version': '2023-06-01',
  'anthropic-beta': BETAS.join(','),
};
const MARK = 'T30C03-turn-';
const TITLE = `t30c03-${process.pid}`;

const directory = fs.mkdtempSync(path.join(os.tmpdir(), 'awaken-t30c03-'));
const data = path.join(directory, 'server-data');
let server;
let upstream;

// ---- deterministic, countable, gateable model --------------------------------
const turns = new Map();
function turnFor(name) {
  if (!turns.has(name)) {
    const turn = { calls: 0 };
    turn.arrived = new Promise((resolve) => { turn.arrive = resolve; });
    turn.released = name === 'B' ? new Promise((resolve) => { turn.release = resolve; }) : Promise.resolve();
    turn.release ??= () => {};
    turns.set(name, turn);
  }
  return turns.get(name);
}
async function behavior(request) {
  const users = (request.messages ?? []).filter((m) => m.role === 'user');
  const content = users.at(-1)?.content ?? '';
  const text = typeof content === 'string' ? content : content.map((b) => b.text ?? '').join('');
  const name = new RegExp(`${MARK}(\\w+)`, 'u').exec(text)?.[1];
  if (!name) return { text: 'auxiliary' };
  const turn = turnFor(name);
  turn.calls += 1;
  turn.arrive();
  await turn.released;
  return { text: `reply-${name}` };
}

// Fail loudly instead of hanging when a causal promise never settles.
function withDeadline(promise, ms, label) {
  let timer;
  const expired = new Promise((_, reject) => { timer = setTimeout(() => reject(new Error(`${label}: not settled in ${ms}ms`)), ms); });
  return Promise.race([promise, expired]).finally(() => clearTimeout(timer));
}

// ---- the lossy SDK transport ----------------------------------------------------
let loseNext = null;
async function lossyFetch(url, init) {
  const response = await fetch(url, init);
  if (loseNext?.(String(url), init)) {
    loseNext = null;
    await response.arrayBuffer();
    throw new TypeError('t30c03: the response was lost after the server committed');
  }
  return response;
}
const sdk = () => new Anthropic({ apiKey: 'e2e-local', baseURL: BASE, maxRetries: 0, fetch: lossyFetch }); // awaken-allow(secret): (local no-login placeholder)

async function raw(method, route, body, key) {
  const response = await fetch(`${BASE}${route}`, {
    method,
    headers: { ...HEADERS, 'content-type': 'application/json', ...(key ? { 'idempotency-key': key } : {}) },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  return { status: response.status, body: await response.json() };
}

async function sessionsTitled(client, title) {
  const sessions = [];
  for await (const session of client.beta.sessions.list({ betas: BETAS })) {
    if (session.title === title) sessions.push(session);
  }
  return sessions;
}

// Count-based checker shared by the real run and the NC.
function duplicateViolations(label, sessions) {
  return sessions.length === 1 ? [] : [`${label}/O1 ${sessions.length} Sessions for one keyed create`];
}

const launch = async () => {
  server = spawnProduction(data, PORT, { fields: { log_filter: 'warn' } }, withoutAmbientProviderKeys(process.env));
  await waitForPort(PORT, 120_000, server);
};

async function main() {
  const key = 't30c03-fixture-key'; // awaken-allow(secret): (synthetic upstream)
  upstream = await startFakeAnthropic(key, { models: ['t30c03-fixture'], behavior });
  await launch();
  const context = await fetch(`${BASE}/v1/config/workspace-context`);
  assert.ok(context.ok, 'workspace context');
  const published = await provisionProviderTestModel(BASE, context.headers.get('anthropic-workspace-id'), {
    id: 't30c03', provider_id: 'anthropic', dialect: 'anthropic_messages', base_url: `${upstream.url}/v1`,
    secret: key, model_id: 't30c03-fixture', timeout_secs: 120,
  });
  let client = sdk();
  const agent = await client.beta.agents.create({ name: 'interruption recovery', model: published.model, system: 'Reply with the configured text.', betas: BETAS });
  const createBody = { agent: { type: 'agent', id: agent.id, version: agent.version }, environment_id: 'env_local', title: TITLE, betas: BETAS };
  const createKey = { headers: { 'Idempotency-Key': `t30c03-create-${process.pid}` } };

  // R1: lost create response, retried with the same key.
  loseNext = (url, init) => init?.method === 'POST' && /\/v1\/sessions(\?|$)/u.test(url);
  await assert.rejects(client.beta.sessions.create(createBody, createKey), /lost after the server committed|Connection error/u, 'R1 the response was lost');
  assert.equal((await sessionsTitled(client, TITLE)).length, 1, 'NC/R1 the fault was injected after the server committed');
  const session = await client.beta.sessions.create(createBody, createKey);
  assert.deepEqual(duplicateViolations('R1', await sessionsTitled(client, TITLE)), [], 'R1 one Session');

  // R2: lost send response, retried with the same key.
  const sendBody = { events: [{ type: 'user.message', content: [{ type: 'text', text: `${MARK}A reply briefly.` }] }], betas: BETAS };
  const sendKey = { headers: { 'Idempotency-Key': `t30c03-send-${process.pid}` } };
  loseNext = (url, init) => init?.method === 'POST' && url.includes(`/v1/sessions/${session.id}/events`);
  await assert.rejects(client.beta.sessions.events.send(session.id, sendBody, sendKey), 'R2 the response was lost');
  const receipt = (await client.beta.sessions.events.send(session.id, sendBody, sendKey)).data[0];
  await waitForSessionEventReceipt(client, session.id, receipt.id, BETAS,
    ({ delta }) => delta.some((e) => e.type === 'session.status_idle'), 'R2 the keyed turn committed its terminal', { timeoutMs: 120_000 });
  let history = await listManagedSessionEvents(client, session.id, BETAS);
  assert.equal(history.filter((e) => e.type === 'user.message' && JSON.stringify(e.content).includes(`${MARK}A`)).length, 1, 'R2/O1 one durable input');
  assert.equal(turnFor('A').calls, 1, 'R2/O1 the inference ran once');

  // R3: drop the event stream while turn B is active, then continue from the cursor.
  const sendingB = client.beta.sessions.events.send(session.id, { events: [{ type: 'user.message', content: [{ type: 'text', text: `${MARK}B reply briefly.` }] }], betas: BETAS });
  sendingB.catch(() => {});
  await withDeadline(turnFor('B').arrived, 120_000, 'R3 turn B reaches the model');
  const abort = new AbortController();
  const seen = [];
  const stream = await client.beta.sessions.events.stream(session.id, { betas: BETAS }, { signal: abort.signal });
  for await (const event of stream) {
    seen.push(event);
    if (event.type === 'user.message' && JSON.stringify(event.content).includes(`${MARK}B`)) break;
  }
  abort.abort();
  const cursor = seen.at(-1).id;
  turnFor('B').release();
  const receiptB = (await sendingB).data[0];
  await waitForSessionEventReceipt(client, session.id, receiptB.id, BETAS,
    ({ delta }) => delta.some((e) => e.type === 'session.status_idle'), 'R3 the detached turn committed its terminal', { timeoutMs: 120_000 });
  history = await listManagedSessionEvents(client, session.id, BETAS);
  const continued = (await raw('GET', `/v1/sessions/${session.id}/events?page=${cursor}&limit=100`)).body.data;
  const at = history.findIndex((e) => e.id === cursor);
  assert.deepEqual(continued.map((e) => e.id), history.slice(at + 1).map((e) => e.id), 'R3/O0 the cursor continues the same prefix');
  const idleB = history.slice(at + 1).filter((e) => e.type === 'session.status_idle');
  assert.equal(idleB.length, 1, 'R3/O0 the Run finished once after the stream dropped (detach)');
  assert.equal(idleB[0].stop_reason?.type, 'end_turn', 'R3/O0 detach, not an interrupt');
  assert.equal(turnFor('B').calls, 1, 'R3/O1 no restarted inference');

  // R4: hard kill + respawn; continue with the original id, keys and cursor.
  const before = await listManagedSessionEvents(client, session.id, BETAS);
  const savedCursor = before[Math.floor(before.length / 2)].id;
  const calls = [...turns.values()].map((t) => t.calls);
  await hardKillServer(server, 'T30-C03 restart');
  server = undefined;
  await launch();
  client = sdk();
  const retrieved = await client.beta.sessions.retrieve(session.id, { betas: BETAS });
  assert.equal(retrieved.id, session.id, 'R4/O0 the Session identity survived');
  assert.equal((await client.beta.sessions.create(createBody, createKey)).id, session.id, 'R4/O0 the create ticket replays');
  const replayed = (await client.beta.sessions.events.send(session.id, sendBody, sendKey)).data[0];
  assert.deepEqual(replayed, receipt, 'R4/O0 the original send receipt replays');
  const after = await listManagedSessionEvents(client, session.id, BETAS);
  assert.deepEqual(after.map((e) => e.id), before.map((e) => e.id), 'R4/O1 no new durable effect after restart');
  const resumed = (await raw('GET', `/v1/sessions/${session.id}/events?page=${savedCursor}&limit=100`)).body.data;
  const savedAt = before.findIndex((e) => e.id === savedCursor);
  assert.deepEqual(resumed.map((e) => e.id), before.slice(savedAt + 1).map((e) => e.id), 'R4/O0 the saved cursor continues the same prefix');
  assert.deepEqual([...turns.values()].map((t) => t.calls), calls, 'R4/O1 no inference restarted');
  assert.deepEqual(duplicateViolations('R4', await sessionsTitled(client, TITLE)), [], 'R4/O1 still one Session');

  // R5: the same replays through the raw HTTP adapter.
  const rawCreate = await raw('POST', '/v1/sessions', { agent: createBody.agent, environment_id: 'env_local', title: TITLE }, createKey.headers['Idempotency-Key']);
  assert.equal(rawCreate.body.id, session.id, 'R5/O2 raw HTTP replays the same Session');
  const rawSend = await raw('POST', `/v1/sessions/${session.id}/events`, { events: sendBody.events }, sendKey.headers['Idempotency-Key']);
  assert.equal(rawSend.body.data?.[0]?.id, receipt.id, 'R5/O2 raw HTTP replays the same receipt');
  pass('T30-C03: lost responses, a dropped stream and a hard restart keep identities, receipts and cursors');

  // NC: a create that does not carry the original ticket is a second effect,
  // and the same checker reports it.
  const ncTitle = `${TITLE}-nc`;
  for (const attempt of [1, 2]) {
    await client.beta.sessions.create({ ...createBody, title: ncTitle }, { headers: { 'Idempotency-Key': `t30c03-nc-${process.pid}-${attempt}` } });
  }
  assert.ok(duplicateViolations('NC', await sessionsTitled(client, ncTitle)).length > 0, 'NC: a retry without the original ticket is detected');
  pass('T30-C03 NC: the checker detects a retry that does not continue the original ticket');
}

try {
  await main();
  console.log('\nE2E PASS: T30-C03 interruption recovery on the production service');
} catch (error) {
  console.error(`E2E FAIL: ${error.stack}`);
  process.exitCode = 1;
} finally {
  for (const turn of turns.values()) turn.release();
  if (server) await stopServer(server);
  if (upstream) await upstream.close();
  cleanupFixtureTree(directory);
}
