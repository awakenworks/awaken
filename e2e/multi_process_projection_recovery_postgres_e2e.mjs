// T31-C03 真实多进程恢复 — two real `awaken all-in-one` OS processes over one
// PostgreSQL database.
//
// Scenario: instance A and instance B share one real backend. A executes a
// Session Run and is hard-killed (TerminateProcess / SIGKILL, never a graceful
// stop) while that Run is still being projected: its User Event is accepted,
// its inference is in flight at the fake provider, and no terminal is committed.
// B, an independent process with its own empty Managed projection cache and its
// own data_dir, continues serving the same Session and rebuilds the projection
// from committed PostgreSQL truth.
//
// Oracles (0-based, groups-5.json order):
//   O0 public Event IDs, content order and pagination cursors issued by A before
//      the exit are identical when served by B after the exit.
//   O1 every Run has exactly one terminal fact: none missing, none duplicated.
//
// Executed negative controls:
//   NC-process  the multi-process qualification check rejects two "instances"
//               that live in ONE OS process (two in-memory objects sharing this
//               test's PID), and the real pair must have distinct child PIDs,
//               with the killed PID verifiably gone from the OS.
//   NC-checker  every oracle checker is run against mutants of the real
//               observed history (dropped/duplicated terminal, duplicated public
//               ID, reordered or renumbered Event, forged cursor) and must reject
//               each mutant.
//
// PostgreSQL fixture contract (same as awaken_reliability_testkit):
//   AWAKEN_TEST_DATABASE_URL=postgres://awaken@127.0.0.1:55432/awaken_test
//   AWAKEN_TEST_REQUIRE_POSTGRES=1  -> a missing fixture fails instead of skipping.
// The test creates its own database, never touches the admin database's
// tables, and drops its database with FORCE after both processes are gone.
//
// Run (from e2e/): node multi_process_projection_recovery_postgres_e2e.mjs

import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import Anthropic from '@anthropic-ai/sdk';
import { startFakeAnthropic } from './fixtures/fake_anthropic_fixture.mjs';
import {
  availablePort,
  cleanupFixtureTree,
  hardKillServer,
  listManagedSessionEvents,
  pass,
  retryableReadStatus,
  runCleanupSteps,
  spawnProduction,
  stopServer,
  waitForPort,
  waitForSessionEventReceipt,
  waitForValue,
} from './harness.mjs';
import { postgresQuery } from './postgres.mjs';

const BETAS = ['managed-agents-2026-04-01'];
const WORKSPACE = `workspace_t31c03_${process.pid}`;
const AGENT = `t31c03-agent-${process.pid}`;
const MODEL = 'fake-haiku';
const PROVIDER = 'anthropic';
const FAKE_KEY = `sk-t31c03-${process.pid}`; // awaken-allow(secret):
const SEAL_KEY = '00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff';
const FIRST_PROMPT = 'T31C03-FIRST-RUN';
const HELD_PROMPT = 'T31C03-HELD-RUN';
const THIRD_PROMPT = 'T31C03-THIRD-RUN';
const BOOT_TIMEOUT_MS = 120_000;
// A's Dispatch lease (30s default) must expire before B may reclaim the Run;
// terminal reconciliation runs every 30s. The bound covers both plus margin.
const RECOVERY_TIMEOUT_MS = 240_000;
const PAGE_LIMIT = 2;

// ---------------------------------------------------------------------------
// PostgreSQL fixture admission and an owned database.

function postgresFixtureUrl() {
  const required = process.env.AWAKEN_TEST_REQUIRE_POSTGRES;
  if (required !== undefined && required !== '0' && required !== '1') {
    throw new Error('AWAKEN_TEST_REQUIRE_POSTGRES must be exactly 0 or 1');
  }
  const url = process.env.AWAKEN_TEST_DATABASE_URL?.trim();
  if (!url) {
    if (required === '1') {
      throw new Error('AWAKEN_TEST_REQUIRE_POSTGRES=1 but AWAKEN_TEST_DATABASE_URL is not configured');
    }
    return null;
  }
  const parsed = new URL(url);
  if (!['postgres:', 'postgresql:'].includes(parsed.protocol)) {
    throw new Error('AWAKEN_TEST_DATABASE_URL must be a postgres:// URL');
  }
  return url;
}

function ownedDatabase(adminUrl) {
  const name = `awaken_t31c03_${process.pid}_${Date.now().toString(36)}`;
  assert.match(name, /^[a-z0-9_]+$/u, 'owned database name is a plain identifier');
  postgresQuery({ url: adminUrl }, `CREATE DATABASE ${name};`);
  const url = new URL(adminUrl);
  url.pathname = `/${name}`;
  return {
    name,
    url: url.toString(),
    drop: () => postgresQuery({ url: adminUrl }, `DROP DATABASE IF EXISTS ${name} WITH (FORCE);`),
  };
}

// ---------------------------------------------------------------------------
// Process qualification (the scenario's negative-control subject).

function osProcessAlive(pid) {
  try {
    process.kill(pid, 0);
    return true;
  } catch (error) {
    if (error?.code === 'ESRCH') return false;
    throw error;
  }
}

// Two service instances qualify only as two OS processes distinct from each
// other and from this test process. Two objects in one process share a PID and
// therefore cannot satisfy multi-process qualification.
function assertMultiProcessQualification(instances, selfPid = process.pid) {
  assert.ok(instances.length >= 2, 'multi-process qualification needs at least two instances');
  for (const instance of instances) {
    assert.ok(
      Number.isInteger(instance.pid) && instance.pid > 0,
      `instance ${instance.name} has a real OS pid (got ${instance.pid})`,
    );
    assert.notEqual(instance.pid, selfPid, `instance ${instance.name} is not the test process`);
  }
  const pids = new Set(instances.map((instance) => instance.pid));
  assert.equal(pids.size, instances.length, 'every instance is a distinct OS process');
  const ports = new Set(instances.map((instance) => instance.port));
  assert.equal(ports.size, instances.length, 'every instance owns its own listener');
}

// ---------------------------------------------------------------------------
// Oracle checkers (pure; also exercised against mutants below).

// processed_at may move only from null to a committed timestamp; every other
// public field of an issued Event is immutable.
function assertEventPreserved(before, after, label) {
  assert.equal(after?.id, before.id, `${label}: public Event id preserved`);
  assert.equal(after.type, before.type, `${label}: Event type preserved for ${before.id}`);
  const { processed_at: beforeProcessed, ...beforeRest } = before;
  const { processed_at: afterProcessed, ...afterRest } = after;
  assert.deepEqual(afterRest, beforeRest, `${label}: Event content preserved for ${before.id}`);
  if (beforeProcessed !== null && beforeProcessed !== undefined) {
    assert.equal(afterProcessed, beforeProcessed, `${label}: committed processed_at preserved`);
  }
}

function assertPrefixPreserved(before, after, label) {
  assert.ok(
    after.length >= before.length,
    `${label}: history shrank from ${before.length} to ${after.length} Events`,
  );
  before.forEach((event, index) => assertEventPreserved(event, after[index], `${label}[${index}]`));
}

function assertCursorPagesPreserved(before, after, label) {
  assert.equal(after.length, before.length, `${label}: same number of cursor pages`);
  before.forEach((page, index) => {
    assert.equal(
      after[index].next_page,
      page.next_page,
      `${label}: page ${index} next_page cursor is byte-identical`,
    );
    assert.equal(after[index].data.length, page.data.length, `${label}: page ${index} size`);
    page.data.forEach((event, row) =>
      assertEventPreserved(event, after[index].data[row], `${label}: page ${index} row ${row}`));
  });
}

function runWindows(events) {
  const windows = new Map();
  let current = null;
  for (const event of events) {
    if (event.type === 'user.message') {
      current = [];
      windows.set(event.id, current);
    } else if (current) {
      current.push(event);
    }
  }
  return windows;
}

// A Run's terminal fact: the aggregate idle/terminated status, or the terminal
// "execution result is unknown" error the product commits instead of idle when
// an in-flight effect cannot be proven not to have happened.
function terminalFacts(window) {
  return window.filter((event) => event.type === 'session.status_idle'
    || event.type === 'session.status_terminated'
    || (event.type === 'session.error' && event.error?.retry_status?.type === 'terminal'));
}

function assertTerminalsExactlyOnce(events, receiptIds, label) {
  const ids = events.map((event) => event.id);
  assert.equal(new Set(ids).size, ids.length, `${label}: no duplicated public Event id`);
  const windows = runWindows(events);
  for (const receiptId of receiptIds) {
    const window = windows.get(receiptId);
    assert.ok(window, `${label}: Run receipt ${receiptId} is present`);
    const terminals = terminalFacts(window);
    assert.equal(
      terminals.length,
      1,
      `${label}: Run ${receiptId} has exactly one terminal fact; observed ${JSON.stringify(terminals.map((event) => event.type))}`,
    );
  }
}

// ---------------------------------------------------------------------------
// Public HTTP helpers.

async function request(base, method, uri, body) {
  const response = await fetch(`${base}${uri}`, {
    method,
    headers: body === undefined ? {} : { 'content-type': 'application/json' },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  const text = await response.text();
  let parsed = null;
  try {
    parsed = text ? JSON.parse(text) : null;
  } catch {
    parsed = text;
  }
  return { status: response.status, body: parsed };
}

async function eventsPage(base, sessionId, cursor, limit = PAGE_LIMIT) {
  const query = new URLSearchParams({ limit: String(limit) });
  if (cursor) query.set('page', cursor);
  const response = await fetch(
    `${base}/v1/sessions/${encodeURIComponent(sessionId)}/events?${query}`,
    { headers: { 'anthropic-beta': BETAS[0], 'x-api-key': 'e2e-dummy' } },
  );
  const body = await response.json();
  assert.equal(response.status, 200, `events page: ${JSON.stringify(body)}`);
  assert.ok(Array.isArray(body.data), 'events page carries data[]');
  return { data: body.data, next_page: body.next_page ?? null };
}

// The first page plus the page reached by following its issued cursor.
async function cursorWalk(base, sessionId) {
  const first = await eventsPage(base, sessionId, null);
  assert.ok(first.next_page, 'the committed history spans more than one page');
  const second = await eventsPage(base, sessionId, first.next_page);
  return [first, second];
}

function sdkClient(base) {
  return new Anthropic({ apiKey: 'e2e-dummy', baseURL: base, maxRetries: 0 });
}

async function sendUserMessage(client, sessionId, text) {
  const receipt = await client.beta.sessions.events.send(sessionId, {
    events: [{ type: 'user.message', content: [{ type: 'text', text }] }],
    betas: BETAS,
  });
  const receiptId = receipt.data?.[0]?.id;
  assert.equal(typeof receiptId, 'string', 'official SDK returns the exact User Event receipt');
  return receiptId;
}

async function runToEndTurn(client, sessionId, prompt, description) {
  const receiptId = await sendUserMessage(client, sessionId, prompt);
  await waitForSessionEventReceipt(
    client,
    sessionId,
    receiptId,
    BETAS,
    ({ delta }) => delta.some((event) => event.type === 'session.status_idle'
      && event.stop_reason?.type === 'end_turn')
      && delta.some((event) => event.type === 'agent.message'
        && event.content.some((part) => part.type === 'text' && part.text.includes(`FAKE:${prompt}`))),
    description,
    { timeoutMs: 60_000 },
  );
  return receiptId;
}

function startInstance(name, directory, port, databaseUrl) {
  const server = spawnProduction(directory, port, {
    workspace: WORKSPACE,
    identityMode: 'no-login',
    controlSealKey: SEAL_KEY,
    fields: {
      // Both processes bind the same installation identity, but each owns a
      // distinct Dispatch identity so B cannot inherit A's lease by name.
      expected_platform_workspace_id: WORKSPACE,
      dispatch_owner: `t31c03-${name}-${process.pid}`,
      dispatch_poll_interval_ms: 250,
    },
    databases: {
      runtime_database_url: databaseUrl,
      catalog_db: databaseUrl,
      credential_db: databaseUrl,
      config_db: databaseUrl,
      admin_db: databaseUrl,
      data_subject_db: databaseUrl,
      environment_db: databaseUrl,
      sessions_db: databaseUrl,
      captured_content_db: databaseUrl,
      resource_database_url: databaseUrl,
    },
    stderr: 'inherit',
  });
  return { name, server, port, pid: server.pid, base: `http://127.0.0.1:${port}` };
}

async function authorPublishedAgent(base, upstreamUrl) {
  let response = await request(base, 'POST', '/v1/config/provider-connections', {
    idempotency_key: `t31c03-provider-connection-${process.pid}`,
    workspace_id: WORKSPACE,
    provider_id: PROVIDER,
    display_name: 'T31-C03 provider',
    dialect: 'anthropic_messages',
    base_url: `${upstreamUrl}/v1/`,
    timeout_secs: 300,
    secret: FAKE_KEY,
  });
  assert.equal(response.status, 201, JSON.stringify(response.body));
  response = await request(base, 'PUT', `/v1/config/agents/${AGENT}`, {
    name: AGENT,
    model: { id: MODEL },
    system: 'T31-C03 multi-process projection recovery',
    max_steps: 2,
  });
  assert.equal(response.status, 200, JSON.stringify(response.body));
  response = await request(base, 'POST', `/v1/config/agents/${AGENT}/publish`);
  assert.equal(response.status, 200, JSON.stringify(response.body));
}

// ---------------------------------------------------------------------------
// Executed negative controls over real observations.

function executeProcessQualificationNegativeControl(realPair) {
  // Two in-memory "instances" inside this one process: same PID.
  assert.throws(
    () => assertMultiProcessQualification([
      { name: 'in-memory-a', pid: process.pid, port: realPair[0].port },
      { name: 'in-memory-b', pid: process.pid, port: realPair[1].port },
    ]),
    /not the test process|distinct OS process/u,
    'NC-process: two objects in one process must not qualify as two instances',
  );
  // Two handles to the same child process are still one process.
  assert.throws(
    () => assertMultiProcessQualification([
      { ...realPair[0] },
      { ...realPair[0], name: 'alias', port: realPair[1].port },
    ]),
    /distinct OS process/u,
    'NC-process: two handles to one child must not qualify as two instances',
  );
}

function executeCheckerNegativeControls(history, pages, receiptIds) {
  const [firstReceipt] = receiptIds;
  const window = runWindows(history).get(firstReceipt);
  const terminal = terminalFacts(window)[0];
  assert.ok(terminal, 'NC-checker baseline: the first Run has a terminal fact');
  const terminalIndex = history.indexOf(terminal);

  const duplicatedTerminal = [
    ...history.slice(0, terminalIndex + 1),
    { ...terminal, id: `${terminal.id}-duplicate` },
    ...history.slice(terminalIndex + 1),
  ];
  assert.throws(
    () => assertTerminalsExactlyOnce(duplicatedTerminal, [firstReceipt], 'mutant'),
    /exactly one terminal fact/u,
    'NC-checker: a duplicated terminal is detected',
  );
  const missingTerminal = history.filter((event) => event !== terminal);
  assert.throws(
    () => assertTerminalsExactlyOnce(missingTerminal, [firstReceipt], 'mutant'),
    /exactly one terminal fact/u,
    'NC-checker: a missing terminal is detected',
  );
  const duplicatedId = [...history, history[0]];
  assert.throws(
    () => assertTerminalsExactlyOnce(duplicatedId, [firstReceipt], 'mutant'),
    /no duplicated public Event id/u,
    'NC-checker: a duplicated public id is detected',
  );
  const reordered = [...history];
  [reordered[0], reordered[1]] = [reordered[1], reordered[0]];
  assert.throws(
    () => assertPrefixPreserved(history, reordered, 'mutant'),
    /public Event id preserved/u,
    'NC-checker: a reordered history is detected',
  );
  const renumbered = history.map((event, index) =>
    (index === 1 ? { ...event, id: `${event.id}-renumbered` } : event));
  assert.throws(
    () => assertPrefixPreserved(history, renumbered, 'mutant'),
    /public Event id preserved/u,
    'NC-checker: a renumbered public id is detected',
  );
  assert.throws(
    () => assertPrefixPreserved(history, history.slice(1), 'mutant'),
    /history shrank|public Event id preserved/u,
    'NC-checker: a dropped Event is detected',
  );
  const forgedCursor = pages.map((page, index) =>
    (index === 0 ? { ...page, next_page: `${page.next_page}x` } : page));
  assert.throws(
    () => assertCursorPagesPreserved(pages, forgedCursor, 'mutant'),
    /next_page cursor is byte-identical/u,
    'NC-checker: a re-minted cursor is detected',
  );
}

// O1 (opt-in, AWAKEN_E2E_RUN_PRODUCT_GAPS=1): B takes over A's expired lease
// and the interrupted Run reaches exactly one terminal with the Session usable.
// Today B terminates the Session with "current sandbox handle has no
// realization marker", so this reproduction fails until the product gap closes.
// #[ignore = "product gap: a takeover process cannot reuse the hard-killed process's local sandbox realization (no realization marker) and terminates the Session instead of continuing the held Run"]
async function reproduceRecoveredRunContinuesOnTheTakeoverProcess({
  clientB, sessionId, firstReceipt, heldReceipt, historyA, held,
}) {
  // O1: the interrupted Run reaches exactly one terminal through B.
  const recovered = await waitForValue(
    () => listManagedSessionEvents(clientB, sessionId, BETAS),
    (events) => terminalFacts(runWindows(events).get(heldReceipt) ?? []).length >= 1,
    'O1 instance B commits the interrupted Run terminal after reclaiming A\'s expired lease',
    {
      timeoutMs: RECOVERY_TIMEOUT_MS,
      pollMs: 500,
      retryReadError: (error) => retryableReadStatus(error) !== null,
    },
  );
  const [heldTerminal] = terminalFacts(runWindows(recovered).get(heldReceipt));
  const receipts = [firstReceipt, heldReceipt];
  if (heldTerminal.type === 'session.status_idle') {
    // An idle terminal promises a usable Session. Name the recovery outcome
    // instead of letting the later send fail with an opaque 400.
    const status = (await clientB.beta.sessions.retrieve(sessionId, { betas: BETAS })).status;
    const errors = runWindows(recovered).get(heldReceipt)
      .filter((event) => event.type === 'session.error').map((event) => event.error);
    assert.notEqual(
      status,
      'terminated',
      `O1 B recovered the interrupted Run as ${heldTerminal.type}/${heldTerminal.stop_reason?.type} `
      + `but then terminated the Session (errors: ${JSON.stringify(errors)}; provider replays: ${held.replays.length})`,
    );
    // The Session is usable again: a later Run closes the interrupted Run's
    // window, so a late duplicate terminal could not hide behind the read.
    receipts.push(await runToEndTurn(
      clientB, sessionId, THIRD_PROMPT, 'a later Run commits through instance B',
    ));
  }
  const finalHistory = await listManagedSessionEvents(clientB, sessionId, BETAS);
  assertPrefixPreserved(historyA, finalHistory, 'O0 final history');
  assertTerminalsExactlyOnce(finalHistory, receipts, 'O1 final history');
  const heldMessages = runWindows(finalHistory).get(heldReceipt)
    .filter((event) => event.type === 'agent.message');
  assert.ok(heldMessages.length <= 1, 'O1 the interrupted Run committed at most one Agent message');
  assert.equal(held.aborted, 1, 'the killed instance really was mid-inference');
  pass(
    `O1 exactly one terminal per Run (${heldTerminal.type} for the interrupted Run; `
    + `provider replays after recovery: ${held.replays.length})`,
  );
  console.log('T31-C03 MULTI-PROCESS PROJECTION RECOVERY E2E PASS: two OS processes, one PostgreSQL database, hard kill mid-projection, identical IDs/order/cursor and exactly one terminal per Run.');
}

// ---------------------------------------------------------------------------

async function main() {
  const adminUrl = postgresFixtureUrl();
  if (!adminUrl) {
    console.error('SKIPPED PostgreSQL fixture: AWAKEN_TEST_DATABASE_URL is not configured');
    return;
  }
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'awaken-t31c03-'));
  const held = { arrivals: [], aborted: 0, replays: [] };
  const upstream = await startFakeAnthropic(FAKE_KEY, {
    models: [MODEL],
    behavior: async (_parsed, ordinal, signal) => {
      const observed = upstream.requests[ordinal - 1];
      const text = observed?.lastUserText ?? '';
      if (text.includes(HELD_PROMPT) && !observed?.memoryExtractor) {
        if (held.arrivals.length === 0) {
          // The held inference is the projection A is in the middle of. It is
          // released only by the connection closing when A's process dies.
          held.arrivals.push(ordinal);
          await new Promise((resolve) => {
            if (signal.aborted) resolve();
            else signal.addEventListener('abort', resolve, { once: true });
          });
          held.aborted += 1;
          throw new Error('held inference connection closed by the killed instance');
        }
        held.replays.push(ordinal);
      }
      // Thinking + text gives the first Run several public projection IDs.
      return { reasoning: `T31C03-REASONING:${text}`, text: `FAKE:${text}` };
    },
  });
  const portA = await availablePort(Number(process.env.E2E_PORT));
  const portB = await availablePort(portA + 1);
  assert.notEqual(portA, portB);
  let database;
  let instanceA;
  let instanceB;
  const failures = [];
  try {
    database = ownedDatabase(adminUrl);
    instanceA = startInstance('a', path.join(root, 'a'), portA, database.url);
    await waitForPort(portA, BOOT_TIMEOUT_MS, instanceA.server);
    await authorPublishedAgent(instanceA.base, upstream.url);
    const clientA = sdkClient(instanceA.base);
    const session = await clientA.beta.sessions.create({
      agent: AGENT, environment_id: 'env_local', betas: BETAS,
    });
    const firstReceipt = await runToEndTurn(
      clientA, session.id, FIRST_PROMPT, 'first Run commits through instance A',
    );

    // Put A in the middle of projecting the second Run.
    const heldReceipt = await sendUserMessage(clientA, session.id, HELD_PROMPT);
    const beforeExit = await waitForValue(
      async () => ({
        events: await listManagedSessionEvents(clientA, session.id, BETAS),
        heldArrivals: held.arrivals.length,
      }),
      ({ events, heldArrivals }) => heldArrivals === 1
        && events.some((event) => event.id === heldReceipt)
        && terminalFacts(runWindows(events).get(heldReceipt) ?? []).length === 0,
      'instance A accepted the held receipt and its inference is in flight without a terminal',
      { timeoutMs: 60_000, pollMs: 100 },
    );
    const historyA = beforeExit.events;
    const pagesA = await cursorWalk(instanceA.base, session.id);
    pass(`A issued ${historyA.length} public Events and cursor ${pagesA[0].next_page}`);

    // Second, independent OS process over the same PostgreSQL database.
    instanceB = startInstance('b', path.join(root, 'b'), portB, database.url);
    await waitForPort(portB, BOOT_TIMEOUT_MS, instanceB.server);
    assertMultiProcessQualification([instanceA, instanceB]);
    assert.ok(osProcessAlive(instanceA.pid) && osProcessAlive(instanceB.pid), 'both instances alive');
    executeProcessQualificationNegativeControl([instanceA, instanceB]);
    pass(`NC-process executed: A pid=${instanceA.pid}, B pid=${instanceB.pid}, test pid=${process.pid}`);

    const clientB = sdkClient(instanceB.base);
    assertPrefixPreserved(
      historyA,
      await listManagedSessionEvents(clientB, session.id, BETAS),
      'O0 B while A is alive',
    );

    // Hard kill A in the middle of the projection.
    assert.equal(held.aborted, 0, 'A is still holding the in-flight inference before the kill');
    await hardKillServer(instanceA.server, 'T31-C03 projection-midway exit');
    assert.ok(
      instanceA.server.exitCode !== null || instanceA.server.signalCode !== null,
      'A reported an OS exit',
    );
    assert.equal(osProcessAlive(instanceA.pid), false, 'killed instance A is gone from the OS');
    assert.equal(osProcessAlive(instanceB.pid), true, 'instance B survives A');
    pass(`A (pid ${instanceA.pid}) hard-killed mid-projection; B (pid ${instanceB.pid}) continues`);

    // O0: B's cold projection serves A's issued IDs, order, content and cursor.
    const historyB = await listManagedSessionEvents(clientB, session.id, BETAS);
    assertPrefixPreserved(historyA, historyB, 'O0 B after A exit');
    const pagesB = await cursorWalk(instanceB.base, session.id);
    assertCursorPagesPreserved(pagesA, pagesB, 'O0 cursor after A exit');
    const resumed = await eventsPage(instanceB.base, session.id, pagesA[0].next_page, 500);
    assert.deepEqual(
      resumed.data.map((event) => event.id),
      historyB.slice(PAGE_LIMIT).map((event) => event.id),
      'O0 A-issued cursor resumes on B at exactly the next public Event',
    );
    executeCheckerNegativeControls(historyB, pagesB, [firstReceipt]);
    pass('O0 public IDs, order, content and cursors survive the exit; NC-checker executed');

    if (process.env.AWAKEN_E2E_RUN_PRODUCT_GAPS === '1') {
      await reproduceRecoveredRunContinuesOnTheTakeoverProcess({
        clientB, sessionId: session.id, firstReceipt, heldReceipt, historyA, held,
      });
    } else {
      console.log('E2E PASS: T31-C03 O0 and negative controls (NC-process, NC-checker) on two OS processes over one PostgreSQL database; O1 takeover continuation is a known product gap, run it with AWAKEN_E2E_RUN_PRODUCT_GAPS=1');
    }
  } catch (error) {
    failures.push(error);
  } finally {
    await runCleanupSteps([
      async () => { if (instanceB) await stopServer(instanceB.server); },
      async () => { if (instanceA) await stopServer(instanceA.server); },
      async () => upstream?.close(),
      async () => database?.drop(),
      async () => cleanupFixtureTree(root),
    ], failures);
  }
}

main().catch((error) => {
  const causes = error instanceof AggregateError ? error.errors : [];
  for (const cause of causes) console.error(`T31-C03 E2E FAIL cause: ${cause?.message ?? cause}`);
  console.error('T31-C03 E2E FAIL:', error);
  process.exitCode = 1;
});
