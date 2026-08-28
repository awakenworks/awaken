// Recoverable database-less Worker vertical slice over real processes.
//
// One Control process owns SQLite truth and dispatch. Worker A reaches an A2A
// Awaiting boundary through a fault proxy that loses one already-applied commit
// receipt and blocks settlement. After Worker A is killed, Worker B reclaims the
// expired epoch, loads the committed recovery snapshot, resumes the exact remote
// context, commits one terminal effect, and settles. The stale A epoch is then
// rejected by the same production claimed-commit route.

import assert from 'node:assert/strict';
import fs, { mkdtempSync } from 'node:fs';
import http, { type IncomingMessage, type ServerResponse } from 'node:http';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { DatabaseSync, type StatementSync } from 'node:sqlite';
import Anthropic from '@anthropic-ai/sdk';
import { spawnServer, stopServer, waitForPort } from './harness.mjs';
import { closeHttpServer } from './http_server.mjs';

const CONTROL_PORT = Number(process.env.E2E_PORT ?? 38834);
const CONTROL = `http://127.0.0.1:${CONTROL_PORT}`;
const AGENT = 'recoverable-remote-worker';
const THREAD_TEXT = 'recoverable remote worker input';
const TERMINAL_MARKER = 'REMOTE-WORKER-RECOVERED';
const CRASH_RECOVERY_TIMEOUT_MS = 120_000;
const sleep = (milliseconds: number): Promise<void> =>
  new Promise((resolve) => setTimeout(resolve, milliseconds));

async function readBody(request: IncomingMessage): Promise<Buffer> {
  const chunks: Buffer[] = [];
  for await (const chunk of request) chunks.push(Buffer.from(chunk));
  return Buffer.concat(chunks);
}

function json(response: ServerResponse, status: number, body: unknown): void {
  response.writeHead(status, { 'content-type': 'application/json' });
  response.end(JSON.stringify(body));
}

async function startA2aPeer(): Promise<{
  endpoint: string;
  sent: Array<{ contextId?: string; text: string }>;
  close: () => Promise<void>;
}> {
  // A2A wire decision: a discriminated Task/Message/TextPart envelope reaches
  // the Awaiting boundary; omitting any `kind` fails closed as a decode error.
  // This recovery scenario needs the former so it can exercise reclaim/resume.
  const sent: Array<{ contextId?: string; text: string }> = [];
  const server = http.createServer(async (request, response) => {
    if (request.method !== 'POST' || request.url !== '/v1/a2a/message:send') {
      json(response, 404, { error: { message: 'unexpected A2A route' } });
      return;
    }
    const body = JSON.parse((await readBody(request)).toString('utf8'));
    const message = body.message ?? {};
    const text = (message.parts ?? [])
      .map((part: { text?: string }) => String(part.text ?? ''))
      .join('');
    sent.push({ contextId: message.contextId, text });
    if (text.startsWith('reservation-recovery-')) {
      json(response, 200, {
        task: {
          kind: 'task',
          id: `reservation-task-${sent.length}`,
          contextId: `reservation-context-${sent.length}`,
          status: {
            state: 'completed',
            message: {
              kind: 'message',
              messageId: `reservation-message-${sent.length}`,
              role: 'agent',
              parts: [{ kind: 'text', text: `completed ${text}` }],
            },
          },
        },
      });
      return;
    }
    if (message.contextId === 'recovery-context') {
      json(response, 200, {
        task: {
          kind: 'task',
          id: 'recovery-finished',
          contextId: 'recovery-context',
          status: {
            state: 'completed',
            message: {
              kind: 'message',
              messageId: 'recovery-terminal-message',
              role: 'agent',
              parts: [{ kind: 'text', text: TERMINAL_MARKER }],
            },
          },
        },
      });
      return;
    }
    json(response, 200, {
      task: {
        kind: 'task',
        id: 'recovery-task',
        contextId: 'recovery-context',
        status: {
          state: 'input-required',
          message: {
            kind: 'message',
            messageId: 'recovery-question',
            role: 'agent',
            parts: [{ kind: 'text', text: 'continue on another worker?' }],
          },
        },
      },
    });
  });
  await new Promise<void>((resolve) => server.listen(0, '127.0.0.1', resolve));
  const address = server.address();
  assert.ok(address && typeof address !== 'string');
  return {
    endpoint: `http://127.0.0.1:${address.port}`,
    sent,
    close: () => closeHttpServer(server),
  };
}

type CapturedCommit = {
  body: any;
  workerId: string;
};

async function startFaultProxy(): Promise<{
  url: string;
  capturedClaim: () => any;
  claims: () => Array<{ workerId: string; claimed: any }>;
  capturedCommit: () => CapturedCommit | undefined;
  commits: () => any[];
  commitAttempts: () => any[];
  settleAttempts: () => any[];
  runActivityAdmissions: () => any[];
  reservationResolutions: () => any[];
  recoverySnapshots: () => Array<{ workerId: string; snapshot: any }>;
  requestCounts: () => Record<string, number>;
  registration: () => any;
  identity: (workerId: string) => any;
  failNextRunActivityAdmission: () => void;
  awaitingSettle: Promise<void>;
  close: () => Promise<void>;
}> {
  let claimed: any;
  const claims: Array<{ workerId: string; claimed: any }> = [];
  let firstCommit: CapturedCommit | undefined;
  const firstOperationAttempts: any[] = [];
  const commits: any[] = [];
  const settleAttempts: any[] = [];
  const runActivityAdmissions: any[] = [];
  const reservationResolutions: any[] = [];
  const recoverySnapshots: Array<{ workerId: string; snapshot: any }> = [];
  let firstOperationId: string | undefined;
  let blockedAwaitingSettle = false;
  let signalAwaitingSettle!: () => void;
  const requestCounts = new Map<string, number>();
  let registration: any;
  const identities = new Map<string, any>();
  let failedRunActivityAdmissions = 0;
  const awaitingSettle = new Promise<void>((resolve) => {
    signalAwaitingSettle = resolve;
  });

  const server = http.createServer(async (request, response) => {
    const requestKey = `${request.method} ${request.url}`;
    requestCounts.set(requestKey, (requestCounts.get(requestKey) ?? 0) + 1);
    const body = await readBody(request);
    const parsed = body.length > 0 ? JSON.parse(body.toString('utf8')) : {};
    const workerId = String(request.headers['x-awaken-worker-id'] ?? '');
    if (request.url === '/v1/worker/register') registration = parsed;
    if (workerId && parsed.identity) identities.set(workerId, parsed.identity);
    if (request.method === 'POST' && request.url === '/v1/worker/dispatch/settle') {
      settleAttempts.push(parsed);
    }
    if (request.method === 'POST' && request.url === '/v1/worker/session/run-activity/admit') {
      runActivityAdmissions.push(parsed);
      if (failedRunActivityAdmissions > 0) {
        failedRunActivityAdmissions -= 1;
        json(response, 503, { error: 'injected Session activity admission outage' });
        return;
      }
    }
    if (request.method === 'POST' && request.url === '/v1/worker/dispatch/reservation/resolve') {
      reservationResolutions.push(parsed);
    }

    if (
      request.method === 'POST' &&
      request.url === '/v1/worker/dispatch/settle' &&
      parsed.outcome === 'Awaiting' &&
      !blockedAwaitingSettle
    ) {
      blockedAwaitingSettle = true;
      signalAwaitingSettle();
      request.once('close', () => response.destroy());
      return;
    }

    let upstream: Response;
    try {
      upstream = await fetch(`${CONTROL}${request.url}`, {
        method: request.method,
        headers: {
          'content-type': request.headers['content-type'] ?? 'application/json',
          ...(workerId ? { 'x-awaken-worker-id': workerId } : {}),
        },
        body: body.length > 0 ? Uint8Array.from(body) : undefined,
      });
    } catch (error) {
      // Worker shutdown can leave a final claim/renew request in this test-only
      // proxy while Control is closing. That transport failure belongs to the
      // caller; it must not escape the async server callback as an unhandled
      // rejection and make an already-passed recovery scenario fail.
      if (!response.destroyed) {
        json(response, 502, { error: { message: String(error) } });
      }
      return;
    }
    const upstreamBody = Buffer.from(await upstream.arrayBuffer());

    if (
      request.method === 'POST' &&
      request.url === '/v1/worker/dispatch/claim' &&
      upstream.ok
    ) {
      const claimResponse = JSON.parse(upstreamBody.toString('utf8'));
      if (claimResponse.claimed) {
        claimed = claimResponse.claimed;
        claims.push({ workerId, claimed });
      }
    }

    if (
      request.method === 'POST' &&
      request.url === '/v1/worker/recovery/snapshot' &&
      upstream.ok
    ) {
      recoverySnapshots.push({
        workerId,
        snapshot: JSON.parse(upstreamBody.toString('utf8')).snapshot,
      });
    }

    if (
      request.method === 'POST' &&
      request.url === '/v1/worker/commit-claimed' &&
      upstream.ok
    ) {
      commits.push(parsed);
      const operationId = JSON.stringify(parsed.operation?.operation_id);
      if (firstOperationId === undefined) {
        firstOperationId = operationId;
        firstCommit = { body: parsed, workerId };
      }
      if (operationId === firstOperationId) firstOperationAttempts.push(parsed);
      if (firstOperationAttempts.length === 1) {
        response.destroy();
        return;
      }
    }

    response.writeHead(upstream.status, {
      'content-type': upstream.headers.get('content-type') ?? 'application/json',
    });
    response.end(upstreamBody);
  });
  await new Promise<void>((resolve) => server.listen(0, '127.0.0.1', resolve));
  const address = server.address();
  assert.ok(address && typeof address !== 'string');
  return {
    url: `http://127.0.0.1:${address.port}`,
    capturedClaim: () => claimed,
    claims: () => claims,
    capturedCommit: () => firstCommit,
    commits: () => commits,
    commitAttempts: () => firstOperationAttempts,
    settleAttempts: () => settleAttempts,
    runActivityAdmissions: () => runActivityAdmissions,
    reservationResolutions: () => reservationResolutions,
    recoverySnapshots: () => recoverySnapshots,
    requestCounts: () => Object.fromEntries(requestCounts),
    registration: () => registration,
    identity: (workerId) => identities.get(workerId),
    failNextRunActivityAdmission: () => {
      failedRunActivityAdmissions += 1;
    },
    awaitingSettle,
    close: () => closeHttpServer(server),
  };
}

async function workerApi(
  workerId: string,
  identity: any,
  route: string,
  body: Record<string, unknown>,
): Promise<{ status: number; body: any }> {
  const response = await fetch(`${CONTROL}${route}`, {
    method: 'POST',
    headers: {
      'content-type': 'application/json',
      'x-awaken-worker-id': workerId,
    },
    body: JSON.stringify({ ...body, identity }),
  });
  return { status: response.status, body: await response.json().catch(() => ({})) };
}

function spawnCredentialIsolatedWorker(
  workerId: string,
  upstream: string,
): ReturnType<typeof spawnServer>['server'] {
  const inheritedEnvironment = Object.fromEntries(
    Object.entries(process.env).filter(([name]) => {
      const normalized = name.toUpperCase();
      return normalized !== 'API_KEY' && !normalized.endsWith('_API_KEY');
    }),
  );
  return spawnServer('echo', 0, {
    SESSION_DEPLOYMENT_INGRESS: 'durable',
    AWAKEN_UPSTREAM_URL: upstream,
    AWAKEN_SCENARIO_ROLE: 'worker',
    AWAKEN_WORKER_ID: workerId,
  }, inheritedEnvironment).server;
}

async function api(method: string, route: string, body?: unknown): Promise<{ status: number; body: any }> {
  const response = await fetch(`${CONTROL}${route}`, {
    method,
    headers: {
      'anthropic-beta': 'managed-agents-2026-04-01',
      ...(body === undefined ? {} : { 'content-type': 'application/json' }),
    },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  return { status: response.status, body: await response.json().catch(() => ({})) };
}

async function publishRemote(endpoint: string): Promise<void> {
  const stored = await api('PUT', `/v1/config/agents/${AGENT}`, {
    id: AGENT,
    system: '',
    max_steps: 2,
    model: {
      id: 'remote-model',
      model_ref: 'remote-model',
      provider_identity_ref: 'remote-peer',
      backend_ref: `a2a:${endpoint}`,
    },
    tools: [],
    plugins: [],
    plugin_config: {},
  });
  assert.equal(stored.status, 200, JSON.stringify(stored.body));
  const published = await api('POST', `/v1/config/agents/${AGENT}/publish`);
  assert.equal(published.status, 200, JSON.stringify(published.body));
}

async function createSession(): Promise<string> {
  const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: CONTROL });
  const created = await client.beta.sessions.create({
    agent: AGENT,
    environment_id: 'env_local',
    betas: ['managed-agents-2026-04-01'],
  });
  return created.id;
}

async function submitSessionUserRun(sessionId: string): Promise<string> {
  const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: CONTROL });
  const receipt = await client.beta.sessions.events.send(sessionId, {
    betas: ['managed-agents-2026-04-01'],
    events: [{
      type: 'user.message',
      content: [{ type: 'text', text: THREAD_TEXT }],
    }],
  });
  assert.equal(typeof receipt.data?.[0]?.id, 'string', 'Managed ingress accepted one User Event');
  const deadline = Date.now() + 30_000;
  while (Date.now() <= deadline) {
    const response = await api('GET', `/v1/durable/threads/${sessionId}/dispatches`);
    const rows = response.body.dispatches ?? [];
    if (rows.length === 1 && typeof rows[0].run_id === 'string') return rows[0].run_id;
    await sleep(25);
  }
  throw new Error(`Managed User Event did not activate one dispatch for ${sessionId}`);
}

function dispatchDatabases(root: string): string[] {
  const pending = [root];
  const databases: string[] = [];
  while (pending.length > 0) {
    const current = pending.pop()!;
    for (const entry of fs.readdirSync(current, { withFileTypes: true })) {
      const candidate = path.join(current, entry.name);
      if (entry.isDirectory()) pending.push(candidate);
      else if (entry.name.endsWith('dispatch.db')) databases.push(candidate);
    }
  }
  return databases;
}

function withSqlite<T>(database: string, operation: (db: DatabaseSync) => T): T {
  // Cause/effect graph: C1=the live Control/Worker owns a concurrent SQLite
  // transaction; C2=fixture mutation uses the same durable database.
  // C1+C2 without a busy timeout -> transient SQLITE_BUSY test failure;
  // C1+C2 with bounded wait -> serialize, or fail after a real 10s deadlock.
  // C3=external sqlite3 is absent; Node's in-process SQLite -> no tool dependency.
  //
  // | Rule | concurrent owner | timeout | in-process | result                 |
  // | S1   | no               | any     | yes        | execute immediately    |
  // | S2   | yes              | absent  | yes        | flaky SQLITE_BUSY      |
  // | S3   | yes              | 10s     | yes        | wait then execute/fail |
  // | S4   | any              | 10s     | no         | unsupported dependency |
  const db = new DatabaseSync(database);
  try {
    db.exec('PRAGMA busy_timeout = 10000');
    return operation(db);
  } finally {
    db.close();
  }
}

function sqliteValue(database: string, statement: string, ...params: any[]): unknown {
  return withSqlite(database, (db) => {
    const row = db.prepare(statement).get(...params) as Record<string, unknown> | undefined;
    return row ? Object.values(row)[0] : undefined;
  });
}

function sqliteRun(database: string, statement: string, ...params: any[]): void {
  withSqlite(database, (db) => {
    (db.prepare(statement) as StatementSync).run(...params);
  });
}

function reservationRequest(base: any, sessionId: string, label: string): any {
  const request = structuredClone(base);
  const runId = `${base.activation.run_id}-reservation-${label}`;
  request.activation.run_id = runId;
  request.activation.thread_id = sessionId;
  request.session_thread_id = sessionId;
  request.session_activity_epoch = null;
  for (const [index, message] of (request.activation.input ?? []).entries()) {
    message.id = `${message.id}-reservation-${label}-${index}`;
    for (const block of message.content ?? []) {
      if (block.type === 'text') block.text = `reservation-recovery-${label}`;
    }
  }
  return request;
}

async function stageReservationCrashBoundary(
  storage: string,
  request: any,
  ttlMs: number,
): Promise<string> {
  const reservationStartedAt = Date.now();
  const reserved = await api('POST', '/v1/scenario/session-run/reserve', {
    request,
    reservation_ttl_ms: ttlMs,
  });
  const reservationObservedAt = Date.now();
  assert.equal(
    reserved.status,
    200,
    `canonical Session reservation command: ${JSON.stringify(reserved.body)}`,
  );
  assert.equal(reserved.body.outcome, 'Reserved', 'fixture stops after the real reservation commit');
  const runId = String(request.activation.run_id);
  const matches = dispatchDatabases(storage).filter(
    (database) => sqliteValue(
      database,
      'SELECT status FROM runtime_dispatch WHERE run_id = ?',
      runId,
    ) !== undefined,
  );
  assert.equal(matches.length, 1, `reservation fixture has one DispatchQueue authority for ${runId}`);
  const persisted = withSqlite(matches[0], (db) => db.prepare(
    `SELECT status, lease_owner, lease_until, ` +
      `json_extract(request, '$.session_activity_epoch') AS activity_epoch ` +
      `FROM runtime_dispatch WHERE run_id = ?`,
  ).get(runId) as Record<string, unknown>);
  assert.deepEqual(
    {
      status: persisted.status,
      lease_owner: persisted.lease_owner,
      activity_epoch: persisted.activity_epoch,
    },
    {
      status: 'reserved',
      lease_owner: null,
      activity_epoch: null,
    },
    `reservation fixture observes exactly one persisted pre-activity intent for ${runId}`,
  );
  const persistedDeadline = Number(persisted.lease_until);
  assert.ok(
    persistedDeadline >= reservationStartedAt + ttlMs &&
      persistedDeadline <= reservationObservedAt + ttlMs,
    `reservation fixture observes the Store-owned TTL conversion for ${runId}`,
  );
  return runId;
}

async function waitForReservationResolution(
  proxy: { reservationResolutions: () => any[] },
  runId: string,
  after: number,
  variant: 'Admitted' | 'Rejected' | 'Retry',
  timeoutMs = 30_000,
): Promise<any> {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() <= deadline) {
    const resolution = proxy.reservationResolutions().slice(after).find(
      (request) => request.claim?.run_id === runId && (
        variant === 'Rejected'
          ? request.resolution === 'Rejected'
          : request.resolution?.[variant] !== undefined
      ),
    );
    if (resolution) return resolution;
    await sleep(25);
  }
  throw new Error(
    `reservation ${runId} did not resolve as ${variant}: ${JSON.stringify(proxy.reservationResolutions().slice(after))}`,
  );
}

async function waitForDispatchGone(thread: string, runId: string, timeoutMs = 30_000): Promise<void> {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() <= deadline) {
    const response = await api('GET', `/v1/durable/threads/${thread}/dispatches`);
    if (!(response.body.dispatches ?? []).some((dispatch: any) => dispatch.run_id === runId)) return;
    await sleep(25);
  }
  throw new Error(`dispatch ${runId} did not settle from ${thread}`);
}

async function waitForStoredDispatchStatus(
  storage: string,
  runId: string,
  status: string,
  timeoutMs = 30_000,
): Promise<string> {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() <= deadline) {
    for (const database of dispatchDatabases(storage)) {
      if (sqliteValue(
        database,
        'SELECT status FROM runtime_dispatch WHERE run_id = ?',
        runId,
      ) === status) return database;
    }
    await sleep(25);
  }
  throw new Error(`dispatch ${runId} did not persist status ${status}`);
}

function removeEmptyManagedResourceEnvelope(root: string, runId: string): void {
  for (const database of dispatchDatabases(root)) {
    const encoded = sqliteValue(
      database,
      'SELECT request FROM runtime_dispatch WHERE run_id = ?',
      runId,
    );
    if (!encoded) continue;
    const request = JSON.parse(String(encoded));
    delete request.session_resources;
    request.placement.required_capabilities =
      request.placement.required_capabilities.filter(
        (capability: string) => capability !== 'session-resources/v1',
      );
    const rewritten = JSON.stringify(request);
    sqliteRun(
      database,
      'UPDATE runtime_dispatch SET request = ? WHERE run_id = ?',
      rewritten,
      runId,
    );
  }
}

function stagePendingInput(root: string, runId: string, ticket: any): void {
  const result = JSON.stringify({ Input: 'resume-after-reclaim' });
  for (const database of dispatchDatabases(root)) {
    sqliteRun(
      database,
      `INSERT INTO runtime_pending ` +
        `(message_id, run_id, thread_id, correlation_id, result, available_at) VALUES (` +
        `?, ?, ?, ?, ?, NULL) ` +
        `ON CONFLICT(message_id) DO NOTHING`,
      'recovery-resume',
      runId,
      String(ticket.thread_id),
      String(ticket.correlation_id),
      result,
    );
  }
}

async function waitForDispatchStatus(
  thread: string,
  runId: string,
  status: string,
  storage: string,
  diagnostics: () => unknown,
  timeoutMs = 30_000,
): Promise<void> {
  const deadline = Date.now() + timeoutMs;
  let lastDispatch: any;
  while (Date.now() <= deadline) {
    const response = await api('GET', `/v1/durable/threads/${thread}/dispatches`);
    const dispatch = (response.body.dispatches ?? []).find(
      (candidate: any) => candidate.run_id === runId,
    );
    lastDispatch = dispatch;
    if (dispatch?.status === status) return;
    await sleep(50);
  }
  const durableRows = dispatchDatabases(storage).map((database) => ({
    database,
    row: withSqlite(database, (db) =>
      db
        .prepare(
          'SELECT status, lease_owner, lease_until, lease_epoch, attempt_count ' +
            'FROM runtime_dispatch WHERE run_id = ?',
        )
        .get(runId),
    ),
  }));
  throw new Error(
    `run ${runId} did not reach ${status}; ` +
      `last_dispatch=${JSON.stringify(lastDispatch)} durable_rows=${JSON.stringify(durableRows)} ` +
      `diagnostics=${JSON.stringify(diagnostics())}`,
  );
}

async function waitForReplacementClaim(
  proxy: {
    registration: () => any;
    claims: () => Array<{ workerId: string; claimed: any }>;
    requestCounts: () => Record<string, number>;
  },
  workerId: string,
  runId: string,
  storage: string,
  timeoutMs = CRASH_RECOVERY_TIMEOUT_MS,
): Promise<void> {
  // Recovery readiness cause/effect graph: C1 the replacement Worker is
  // registered; C2 it claims the exact expired Run at a higher epoch; C3 the
  // frozen backend is remote A2A and requires no local Environment. C1+C2 is
  // the recovery linearization evidence. C3 -> zero sandbox binds is valid and
  // must not block the scenario; a local-Sandbox test owns binding/adoption.
  //
  // C4 the Coordinator-only terminal scanner shares the Dispatch store but may
  // claim only committed Ended Runs. C1+C2+C4 is active-active evidence that a
  // maintenance scan cannot steal a non-terminal expired lease or hide the theft
  // by restoring the same public status.
  //
  // | Rule | C1 registered | C2 exact claim | C3 remote-only | C4 nonterminal scan | ready | bind required |
  // | R1   | T             | T              | T              | preserves row       | T     | F             |
  const deadline = Date.now() + timeoutMs;
  while (Date.now() <= deadline) {
    const registered = proxy.registration()?.registration?.worker_id === workerId;
    const reclaimed = proxy.claims().some(
      (entry) => entry.workerId === workerId && entry.claimed?.lease?.run_id === runId,
    );
    if (registered && reclaimed) return;
    await sleep(50);
  }
  throw new Error(
    `replacement Worker ${workerId} did not claim Run ${runId}; ` +
      `registration=${JSON.stringify(proxy.registration())} ` +
      `claims=${JSON.stringify(proxy.claims())} ` +
      `requests=${JSON.stringify(proxy.requestCounts())} ` +
      `durable=${JSON.stringify(
        dispatchDatabases(storage).map((database) => ({
          database,
          row: withSqlite(database, (db) =>
            db
              .prepare(
                'SELECT status, lease_owner, lease_until, lease_epoch, attempt_count, ' +
                  'worker_assignment, credential_bindings FROM runtime_dispatch WHERE run_id = ?',
              )
              .get(runId),
          ),
          operations: withSqlite(database, (db) =>
            db
              .prepare(
                'SELECT operation FROM runtime_dispatch_operation ' +
                  'WHERE operation LIKE ? ORDER BY rowid',
              )
              .all(`%${runId}%`),
          ),
        })),
      )}`,
  );
}

async function waitForTerminalMessage(
  thread: string,
  diagnostics: () => unknown,
  timeoutMs = CRASH_RECOVERY_TIMEOUT_MS,
): Promise<any[]> {
  const deadline = Date.now() + timeoutMs;
  let messages: any[] = [];
  while (Date.now() <= deadline) {
    const response = await api('GET', `/v1/durable/threads/${thread}/messages`);
    messages = response.body.messages ?? [];
    if (JSON.stringify(messages).includes(TERMINAL_MARKER)) return messages;
    await sleep(50);
  }
  throw new Error(
    `terminal message was not committed: ${JSON.stringify(messages)} ` +
      `diagnostics=${JSON.stringify(diagnostics())}`,
  );
}

async function main(): Promise<void> {
  // Topology/recovery cause-effect graph: C1 disables the Coordinator's local
  // pool; C2 admits a Managed Session through the same process; C3 Worker A
  // owns the current claim; C4 its applied commit receipt is lost; C5 A dies;
  // C6 Worker B reclaims at a higher epoch. C1 must freeze Worker placement in
  // the Session application (not merely suppress the Host pool), so C2 creates
  // no competing local realization lease. C3+C4 retries one operation id;
  // C5+C6 first reclaims the subordinate Run epoch; C7 then waits for the
  // independently durable Session Work lease to expire before realization can
  // transfer. C5+C6+C7 resumes the committed snapshot, fences A, and emits one
  // terminal fact. C8 keeps the Coordinator-only terminal scanner active over
  // the same Dispatch store; it may repair Ended truth but must not claim this
  // committed-Awaiting Run. C9 requires the registered replacement to pass the
  // frozen remote placement capabilities, and C10 verifies its credential-free
  // attempt remains an exact empty binding rather than bypassing credential
  // admission. C11 requires both Worker processes to complete their immediate
  // Environment warmup reconciliation over this same private transport; a
  // missing Scenario route returns a non-warmup body and the Worker reports the
  // response-decode failure instead of exercising production topology. The
  // scenario WorkQueue is intentionally in-memory, so time
  // is the production crash-recovery authority; directly mutating a second
  // store would create a false parallel ownership path.
  //
  // | Rule | remote placement | credential binding | claim owner | Work TTL | Effect |
  // | T1   | compatible       | exact empty        | B after A crash | elapsed | both fences transfer; one terminal fact |
  // | T2   | incompatible     | any                | none       | any      | Pending (Rust placement tests) |
  // | T3   | compatible       | exact              | stale A    | any      | old commit rejected |
  // | T4   | compatible       | exact              | current A  | live     | ordinary single-Worker completion |
  // | T5   | compatible       | exact              | B after A crash | live  | realization remains fenced until Work expiry |
  // | T6   | compatible       | exact              | coordinator scan | any | nonterminal Run is preserved for B |
  // | T7   | compatible       | exact              | A and B warmup clients | any | both decode the canonical warmup projection |
  // FMECA: bounding terminal recovery below the 60-second Work TTL makes T5 a
  // false failure and masks T1. The shared 120-second budget covers one exact
  // expiry plus the 30-second durable-pool retry cadence without weakening the
  // stale-owner or exactly-once assertions. Constraints/invariant: Dispatch
  // claim epoch, Session Work lease, and frozen placement/credential binding are
  // independent authorities and all must transfer before B may commit.
  const storage = mkdtempSync(path.join(tmpdir(), 'awaken-remote-worker-recovery-'));
  const peer = await startA2aPeer();
  const proxy = await startFaultProxy();
  const control = spawnServer('config', CONTROL_PORT, {
    SESSION_DEPLOYMENT_INGRESS: 'durable',
    SESSION_DEPLOYMENT_STORAGE_DIR: storage,
    SESSION_DEPLOYMENT_DISABLE_LOCAL_POOL: '1',
  }).server;
  let workerA: ReturnType<typeof spawnServer>['server'] | undefined;
  let workerB: ReturnType<typeof spawnServer>['server'] | undefined;
  try {
    await waitForPort(CONTROL_PORT, 180_000, control);
    await publishRemote(peer.endpoint);
    const thread = await createSession();
    const runId = await submitSessionUserRun(thread);
    // Managed Session creation intentionally pins an explicit empty resource
    // envelope. This Worker-recovery scenario has no resource plane, so strip
    // that unrelated fixture dimension directly from the disposable dispatch.
    removeEmptyManagedResourceEnvelope(storage, runId);
    workerA = spawnCredentialIsolatedWorker('recovery-worker-a', proxy.url);
    await Promise.race([
      proxy.awaitingSettle,
      // Cause graph: lost applied-commit receipt -> retry with the same
      // idempotency identity -> Awaiting settlement. Instrumented Windows builds
      // can spend more than 10s in the bounded transport backoff, so use the
      // suite's normal durable-state budget while retaining exact attempt checks.
      sleep(30_000).then(async () => {
        const dispatches = await api('GET', `/v1/durable/threads/${thread}/dispatches`);
        const requests = dispatchDatabases(storage).map((database) =>
          sqliteValue(
            database,
            'SELECT request FROM runtime_dispatch WHERE run_id = ?',
            runId,
          ),
        );
        throw new Error(
          `Worker A did not attempt Awaiting settle; proxy=${JSON.stringify(proxy.requestCounts())} ` +
            `registration=${JSON.stringify(proxy.registration())} ` +
            `settles=${JSON.stringify(proxy.settleAttempts())} ` +
            `claim=${JSON.stringify(proxy.capturedClaim())} commit=${JSON.stringify(proxy.capturedCommit())} ` +
            `peer=${JSON.stringify(peer.sent)} ` +
            `request=${JSON.stringify(requests)} dispatches=${JSON.stringify(dispatches.body)}`,
        );
      }),
    ]);

    const claimA = proxy.capturedClaim();
    const staleCommit = proxy.capturedCommit();
    assert.equal(claimA?.lease?.run_id, runId, 'Worker A held the submitted Run');
    assert.ok(staleCommit, 'Worker A emitted a claimed commit');
    assert.equal(proxy.commitAttempts().length, 2, 'lost receipt retried exactly once');
    assert.deepEqual(
      proxy.commitAttempts()[0],
      proxy.commitAttempts()[1],
      'the retry preserved operation id, version, hash, and payload',
    );

    // Claimed Managed-Session coordination cause/effect design:
    // C1 the request carries Worker A's exact registered incarnation; C2 its
    // Run claim owner/epoch is live; C3 Session/Thread/Run equal the frozen
    // primary dispatch; C4 the latest committed Run is Awaiting. Effects:
    // E1 roster lookup reaches the one Session application port; E2 model
    // admission returns the aggregate budget decision; E3 a forged owner,
    // stale/expired epoch, or foreign Session is rejected before either
    // application effect. K1 the blocked Awaiting settlement keeps C2 stable
    // while these requests run; the dispatch row remains the sole claim
    // authority. K2 pauses only the throwaway Worker process while the fixture
    // advances its durable lease beyond expiry, then restores the exact value;
    // this is the same crash-clock seam used by the recovery half below.
    //
    // | Rule | identity | claim | coordinates | committed Run | Effect |
    // | A1 | exact | live | exact | Awaiting | E1 |
    // | A2 | exact | live | exact | Awaiting | E2 |
    // | A3 | exact | forged owner | exact | any | E3 |
    // | A4 | exact | stale epoch | exact | any | E3 |
    // | A5 | exact | live | foreign Session | any | E3 |
    // | A6 | exact | expired | exact | any | E3 |
    const identityA = proxy.identity('recovery-worker-a');
    assert.ok(identityA, 'Worker A exposed its registered incarnation on the canonical transport');
    const liveClaim = {
      run_id: claimA.lease.run_id,
      owner: claimA.lease.owner,
      epoch: claimA.lease.epoch,
    };
    const roster = await workerApi(
      'recovery-worker-a',
      identityA,
      '/v1/worker/session/agents/list',
      { claim: liveClaim, session_id: thread },
    );
    assert.equal(roster.status, 200, `A1 exact live claim lists the frozen roster: ${JSON.stringify(roster.body)}`);
    assert.ok(Array.isArray(roster.body.agents), 'A1 returns the canonical roster envelope');
    const modelAdmission = await workerApi(
      'recovery-worker-a',
      identityA,
      '/v1/worker/session/model-request/admit',
      {
        claim: liveClaim,
        session_id: thread,
        thread_id: claimA.request.activation.thread_id,
        run_id: runId,
      },
    );
    assert.equal(
      modelAdmission.status,
      200,
      `A2 exact Awaiting Run reaches model admission: ${JSON.stringify(modelAdmission.body)}`,
    );
    assert.equal(typeof modelAdmission.body.admitted, 'boolean', 'A2 returns the aggregate budget decision');
    for (const [rule, claim, sessionId] of [
      ['A3', { ...liveClaim, owner: 'forged-worker-owner' }, thread],
      ['A4', { ...liveClaim, epoch: liveClaim.epoch + 1 }, thread],
      ['A5', liveClaim, `${thread}-foreign`],
    ] as const) {
      const rejected = await workerApi(
        'recovery-worker-a',
        identityA,
        '/v1/worker/session/agents/list',
        { claim, session_id: sessionId },
      );
      assert.equal(rejected.status, 400, `${rule} is fenced before roster lookup: ${JSON.stringify(rejected.body)}`);
    }
    const authorityDatabase = dispatchDatabases(storage).find(
      (database) => sqliteValue(
        database,
        'SELECT lease_until FROM runtime_dispatch WHERE run_id = ?',
        runId,
      ) !== undefined,
    );
    assert.ok(authorityDatabase, 'A6 locates the one disposable durable dispatch authority');
    const liveLeaseUntil = sqliteValue(
      authorityDatabase,
      'SELECT lease_until FROM runtime_dispatch WHERE run_id = ?',
      runId,
    );
    assert.equal(typeof liveLeaseUntil, 'number', 'A6 starts from a persisted live lease');
    workerA.kill('SIGSTOP');
    try {
      sqliteRun(
        authorityDatabase,
        'UPDATE runtime_dispatch SET lease_until = 0 WHERE run_id = ?',
        runId,
      );
      const expired = await workerApi(
        'recovery-worker-a',
        identityA,
        '/v1/worker/session/agents/list',
        { claim: liveClaim, session_id: thread },
      );
      assert.equal(expired.status, 400, `A6 expired claim is fenced: ${JSON.stringify(expired.body)}`);
    } finally {
      sqliteRun(
        authorityDatabase,
        'UPDATE runtime_dispatch SET lease_until = ? WHERE run_id = ?',
        liveLeaseUntil,
        runId,
      );
      workerA.kill('SIGCONT');
    }

    const killedA = new Promise<void>((resolve) => workerA!.once('exit', () => resolve()));
    workerA.kill('SIGKILL');
    await killedA;
    workerA = undefined;
    const databases = dispatchDatabases(storage);
    assert.ok(databases.length > 0, 'durable dispatch database exists');
    for (const database of databases) {
      sqliteRun(
        database,
        'UPDATE runtime_dispatch SET lease_until = 0 WHERE run_id = ?',
        runId,
      );
    }

    workerB = spawnCredentialIsolatedWorker('recovery-worker-b', proxy.url);
    await waitForReplacementClaim(
      proxy,
      'recovery-worker-b',
      runId,
      storage,
    );
    const claimB = proxy
      .claims()
      .find(
        (entry) =>
          entry.workerId === 'recovery-worker-b' && entry.claimed?.lease?.run_id === runId,
      )!.claimed;
    assert.equal(claimB.recovered, true, 'Worker B used the expired-lease recovery path');
    assert.equal(
      claimB.assignment?.identity?.worker_id,
      'recovery-worker-b',
      'frozen remote placement selected the authenticated replacement',
    );
    assert.equal(
      claimB.request.placement.location,
      'remote_required',
      'the replacement did not weaken remote placement',
    );
    assert.deepEqual(
      claimB.credential_bindings,
      [],
      'the credential-free A2A attempt used the canonical empty binding',
    );
    const epochB = Math.max(
      ...databases.map((database) =>
        Number(
          sqliteValue(
            database,
            'SELECT lease_epoch FROM runtime_dispatch WHERE run_id = ?',
            runId,
          ) || 0,
        ),
      ),
    );
    assert.ok(epochB > claimA.lease.epoch, 'Worker B recovery advanced the claim epoch');

    const stale = await fetch(`${CONTROL}/v1/worker/commit-claimed`, {
      method: 'POST',
      headers: {
        'content-type': 'application/json',
        'x-awaken-worker-id': staleCommit!.workerId,
      },
      body: JSON.stringify(staleCommit!.body),
    });
    assert.ok(stale.status >= 400, `old epoch was rejected: ${stale.status}`);

    const awaitingCommit = proxy
      .commits()
      .findLast((commit) => commit.operation?.commit?.resume_ticket);
    assert.ok(awaitingCommit, 'Worker A committed the durable resume ticket');
    stagePendingInput(storage, runId, awaitingCommit.operation.commit.resume_ticket);
    const messages = await waitForTerminalMessage(thread, () => ({
      claims: proxy
        .claims()
        .filter((entry) => entry.workerId === 'recovery-worker-b')
        .slice(-3)
        .map((entry) => ({
          runId: entry.claimed?.lease?.run_id,
          epoch: entry.claimed?.lease?.epoch,
          recovered: entry.claimed?.recovered,
          pending: entry.claimed?.pending?.map((input: any) => input.correlation_id),
        })),
      snapshots: proxy
        .recoverySnapshots()
        .filter((entry) => entry.workerId === 'recovery-worker-b')
        .slice(-3)
        .map((entry) => ({
          runId: entry.snapshot?.claimed_run_id,
          runs: entry.snapshot?.runs,
          tickets: entry.snapshot?.resume_tickets,
        })),
      requests: proxy.requestCounts(),
      peer: peer.sent,
    }));
    const recoveredSnapshot = proxy
      .recoverySnapshots()
      .findLast((entry) => entry.workerId === 'recovery-worker-b')?.snapshot;
    assert.equal(recoveredSnapshot?.claimed_run_id, runId, 'B loaded the claimed Run prefix');
    assert.equal(
      recoveredSnapshot?.runs?.find((run: any) => run.id === runId)?.state,
      'Awaiting',
      'B observed Awaiting truth before choosing resume',
    );
    assert.equal(
      recoveredSnapshot?.resume_tickets?.find((entry: any) => entry.run_id === runId)?.ticket
        ?.correlation_id,
      awaitingCommit.operation.commit.resume_ticket.correlation_id,
      'snapshot install and resume selection shared the exact committed ticket',
    );
    assert.equal(
      messages.filter((message) => String(message.text ?? '').includes(TERMINAL_MARKER)).length,
      1,
      'terminal logical effect committed exactly once',
    );
    assert.equal(
      peer.sent.filter((message) => message.text.includes(THREAD_TEXT)).length,
      1,
      'recovery did not resend the initial remote message',
    );
    assert.deepEqual(
      peer.sent.find((message) => message.text === 'resume-after-reclaim')?.contextId,
      'recovery-context',
      'replacement resumed the committed remote context',
    );
    assert.ok(
      (proxy.requestCounts()['POST /v1/worker/environment/warmups'] ?? 0) >= 2,
      'both Worker processes decoded the canonical Environment warmup response',
    );

    // Reservation recovery R1 cause/effect design:
    // C1 one canonical, self-affine, epochless RunDispatch is already durable;
    // C2 its pre-activity reservation deadline expired; C3 the registered
    // replacement owns the recovery claim; C4 Session admission returns a
    // nonzero activity epoch. Effects: E1 the recovery claim is admission-only;
    // E2 the Worker calls RecoverOrAdmit through the claimed Session port; E3
    // the same row is resolved to Pending with that exact epoch; E4 only its
    // later ordinary claim enters A2A execution and settles once. Constraints:
    // the fixture invokes the real reservation command and then reads its row;
    // every claim, activity decision, resolution, execution, and settlement is
    // owned by the real Worker/Coordinator/Session/Dispatch paths.
    //
    // | Rule | expired | cancel | admission | resolution | Effect |
    // | R1 | yes | no | Admitted(epoch>0) | current claim | E1-E4 |
    // | R3 | yes | no | Unavailable, then Admitted | Retry, then current claim | no execution before higher-epoch E1-E4 |
    await waitForDispatchGone(thread, runId);
    const identityB = proxy.identity('recovery-worker-b');
    assert.ok(identityB, 'R1 replacement exposed its registered incarnation');
    let workerBPaused = false;
    try {
      workerB!.kill('SIGSTOP');
      workerBPaused = true;
      const request = reservationRequest(claimA.request, thread, 'admit');
      const resolutionStart = proxy.reservationResolutions().length;
      const admissionStart = proxy.runActivityAdmissions().length;
      const reservationRunId = await stageReservationCrashBoundary(
        storage,
        request,
        1,
      );
      workerB!.kill('SIGCONT');
      workerBPaused = false;
      const resolved = await waitForReservationResolution(
        proxy,
        reservationRunId,
        resolutionStart,
        'Admitted',
      );
      const admission = proxy.runActivityAdmissions().slice(admissionStart).find(
        (candidate) => candidate.run_id === reservationRunId,
      );
      assert.equal(admission?.mode, 'RecoverOrAdmit', 'R1 non-cancelled recovery uses RecoverOrAdmit');
      assert.equal(admission?.session_id, thread, 'R1 admission keeps the Managed Session affinity');
      assert.ok(
        resolved.resolution.Admitted.session_activity_epoch > 0,
        'R1 resolves with the exact nonzero Session activity epoch',
      );
      await waitForDispatchGone(thread, reservationRunId, CRASH_RECOVERY_TIMEOUT_MS);
      assert.equal(
        peer.sent.filter((message) => message.text === 'reservation-recovery-admit').length,
        1,
        'R1 admission-only recovery is followed by exactly one ordinary execution',
      );

      workerB!.kill('SIGSTOP');
      workerBPaused = true;
      const retryRequest = reservationRequest(claimA.request, thread, 'retry');
      const retryResolutionStart = proxy.reservationResolutions().length;
      const retryAdmissionStart = proxy.runActivityAdmissions().length;
      proxy.failNextRunActivityAdmission();
      const retryRunId = await stageReservationCrashBoundary(
        storage,
        retryRequest,
        1,
      );
      workerB!.kill('SIGCONT');
      workerBPaused = false;
      const retried = await waitForReservationResolution(
        proxy,
        retryRunId,
        retryResolutionStart,
        'Retry',
      );
      const failedAdmission = proxy.runActivityAdmissions().slice(retryAdmissionStart).find(
        (candidate) => candidate.run_id === retryRunId,
      );
      assert.equal(failedAdmission?.mode, 'RecoverOrAdmit', 'R3 first recovery uses RecoverOrAdmit');
      assert.equal(
        peer.sent.filter((message) => message.text === 'reservation-recovery-retry').length,
        0,
        'R3 admission outage never enters execution',
      );
      workerB!.kill('SIGSTOP');
      workerBPaused = true;
      const retryDatabase = await waitForStoredDispatchStatus(storage, retryRunId, 'reserved');
      const retryDeadline = Number(sqliteValue(
        retryDatabase,
        'SELECT lease_until FROM runtime_dispatch WHERE run_id = ?',
        retryRunId,
      ));
      const retryTtl = Number(retried.resolution.Retry.reservation_ttl_ms);
      assert.ok(retryTtl > 0, 'R3 Worker requests a nonzero relative retry TTL');
      assert.ok(
        retryDeadline > retryTtl,
        'R3 store authority converts the relative TTL to its own absolute deadline',
      );
      const firstRetryEpoch = Number(retried.claim.epoch);
      sqliteRun(
        retryDatabase,
        'UPDATE runtime_dispatch SET lease_until = 1 WHERE run_id = ? AND status = ?',
        retryRunId,
        'reserved',
      );
      const recoveredResolutionStart = proxy.reservationResolutions().length;
      workerB!.kill('SIGCONT');
      workerBPaused = false;
      const recovered = await waitForReservationResolution(
        proxy,
        retryRunId,
        recoveredResolutionStart,
        'Admitted',
      );
      assert.ok(
        Number(recovered.claim.epoch) > firstRetryEpoch,
        'R3 expired retry is recovered under a strictly higher claim epoch',
      );
      await waitForDispatchGone(thread, retryRunId, CRASH_RECOVERY_TIMEOUT_MS);
      assert.equal(
        peer.sent.filter((message) => message.text === 'reservation-recovery-retry').length,
        1,
        'R3 retry recovery executes the same Run exactly once',
      );
    } finally {
      if (workerBPaused) workerB!.kill('SIGCONT');
    }

    console.log(
      'REMOTE WORKER RECOVERY TS E2E PASS: warmup decode, lost receipt retry, crash/reclaim, snapshot resume, stale-epoch fence, and exactly-one terminal effect.',
    );
  } finally {
    if (workerA) await stopServer(workerA).catch(() => {});
    if (workerB) await stopServer(workerB).catch(() => {});
    await stopServer(control).catch(() => {});
    await proxy.close().catch(() => {});
    await peer.close().catch(() => {});
    fs.rmSync(storage, { recursive: true, force: true });
  }
}

main().catch((error) => {
  console.error('REMOTE WORKER RECOVERY TS E2E FAIL:', error);
  process.exitCode = 1;
});
