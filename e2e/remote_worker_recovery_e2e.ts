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
  requestCounts: () => Record<string, number>;
  registration: () => any;
  awaitingSettle: Promise<void>;
  close: () => Promise<void>;
}> {
  let claimed: any;
  const claims: Array<{ workerId: string; claimed: any }> = [];
  let firstCommit: CapturedCommit | undefined;
  const firstOperationAttempts: any[] = [];
  const commits: any[] = [];
  const settleAttempts: any[] = [];
  let firstOperationId: string | undefined;
  let blockedAwaitingSettle = false;
  let signalAwaitingSettle!: () => void;
  const requestCounts = new Map<string, number>();
  let registration: any;
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
    if (request.method === 'POST' && request.url === '/v1/worker/dispatch/settle') {
      settleAttempts.push(parsed);
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
        body: body.length > 0 ? body : undefined,
      });
    } catch (error) {
      // Worker shutdown can leave a final claim/renew request in this test-only
      // proxy while Control is closing. That transport failure belongs to the
      // caller; it must not escape the async server callback as an unhandled
      // rejection and turn an already-passed recovery scenario into a failure.
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
    requestCounts: () => Object.fromEntries(requestCounts),
    registration: () => registration,
    awaitingSettle,
    close: () => closeHttpServer(server),
  };
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
  const created = await api('POST', '/v1/sessions', {
    agent: AGENT,
    environment_id: 'env_local',
  });
  assert.equal(created.status, 200, JSON.stringify(created.body));
  return created.body.id;
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
  timeoutMs = CRASH_RECOVERY_TIMEOUT_MS,
): Promise<void> {
  // Recovery readiness cause/effect graph: C1 the replacement Worker is
  // registered; C2 it claims the exact expired Run at a higher epoch; C3 the
  // frozen backend is remote A2A and requires no local Environment. C1+C2 is
  // the recovery linearization evidence. C3 -> zero sandbox binds is valid and
  // must not block the scenario; a local-Sandbox test owns binding/adoption.
  //
  // | Rule | C1 registered | C2 exact claim | C3 remote-only | ready | bind required |
  // | R1   | T             | T              | T              | T     | F             |
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
      `requests=${JSON.stringify(proxy.requestCounts())}`,
  );
}

async function waitForTerminalMessage(
  thread: string,
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
  throw new Error(`terminal message was not committed: ${JSON.stringify(messages)}`);
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
  // terminal fact. The scenario WorkQueue is intentionally in-memory, so time
  // is the production crash-recovery authority; directly mutating a second
  // store would create a false parallel ownership path.
  //
  // | Rule | remote | current | lost receipt | crash/reclaim | Work TTL | Effect |
  // | T1   | yes    | yes     | yes          | yes           | elapsed  | idempotent retry; both fences transfer; one terminal fact |
  // | T2   | no     | n/a     | n/a          | n/a           | n/a      | local phase driver owns realization (Rust placement tests) |
  // | T3   | yes    | stale   | any          | reclaimed     | any      | old commit rejected |
  // | T4   | yes    | yes     | no           | no            | live     | ordinary single-worker completion (worker transport E2E) |
  // | T5   | yes    | yes     | yes          | reclaimed     | live     | replacement realization remains fenced until expiry |
  // FMECA: bounding terminal recovery below the 60-second Work TTL makes T5 a
  // false failure and masks T1. The shared 120-second budget covers one exact
  // expiry plus the 30-second durable-pool retry cadence without weakening the
  // stale-owner or exactly-once assertions.
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
    const submitted = await api('POST', `/v1/durable/threads/${thread}/submit_background`, {
      agent: AGENT,
      text: THREAD_TEXT,
    });
    assert.equal(submitted.status, 200, JSON.stringify(submitted.body));
    const runId = submitted.body.run_id as string;
    // Managed Session creation intentionally pins an explicit empty resource
    // envelope. This Worker-recovery scenario has no resource plane, so strip
    // that unrelated fixture dimension directly from the disposable dispatch.
    removeEmptyManagedResourceEnvelope(storage, runId);
    workerA = spawnServer('echo', 0, {
      SESSION_DEPLOYMENT_INGRESS: 'durable',
      AWAKEN_UPSTREAM_URL: proxy.url,
      AWAKEN_SCENARIO_ROLE: 'worker',
      AWAKEN_WORKER_ID: 'recovery-worker-a',
    }).server;
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

    workerB = spawnServer('echo', 0, {
      SESSION_DEPLOYMENT_INGRESS: 'durable',
      AWAKEN_UPSTREAM_URL: proxy.url,
      AWAKEN_SCENARIO_ROLE: 'worker',
      AWAKEN_WORKER_ID: 'recovery-worker-b',
    }).server;
    await waitForReplacementClaim(
      proxy,
      'recovery-worker-b',
      runId,
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
    const messages = await waitForTerminalMessage(thread);
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

    console.log(
      'REMOTE WORKER RECOVERY TS E2E PASS: lost receipt retry, crash/reclaim, snapshot resume, stale-epoch fence, and exactly-one terminal effect.',
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
