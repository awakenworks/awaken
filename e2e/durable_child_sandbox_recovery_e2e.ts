// Cross-process durable child + sandbox recovery over the real server binary.
//
// The parent delegates to a first-class child Run. We crash while the child is
// executing with a bound sandbox, expire the abandoned SQLite lease (equivalent
// to passage of lease time), restart on the same durable directory, and assert:
// the stable child identity is reused, its sandbox handle is adopted unchanged,
// the result resumes the parent, and no duplicate child relationship appears.

import assert from 'node:assert/strict';
import fs, { mkdtempSync } from 'node:fs';
import http from 'node:http';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { DatabaseSync } from 'node:sqlite';
import Anthropic from '@anthropic-ai/sdk';
// @ts-ignore -- shared JavaScript harness intentionally serves TS scenarios.
import { FAKE_KEY, realServerEnv, spawnServer, stopServer, waitForPort } from './harness.mjs';
// @ts-ignore -- shared JavaScript fixture intentionally serves TS scenarios.
import { startFakeAnthropic } from './fixtures/fake_anthropic_fixture.mjs';
// @ts-ignore -- shared JavaScript SQLite fixture intentionally serves TS scenarios.
import { sqliteDatabaseForThread } from './sqlite.mjs';

type DispatchRow = {
  run_id: string;
  thread_id: string;
  status: string;
  lease_until: number | null;
  sandbox: string | null;
  request: string;
};

const PORT = Number(process.env.E2E_PORT ?? 39661);
const BASE = `http://127.0.0.1:${PORT}`;
const BETAS = ['managed-agents-2026-04-01'];
const sleep = (ms: number): Promise<void> => new Promise((resolve) => setTimeout(resolve, ms));

function filesUnder(root: string): string[] {
  const pending = [root];
  const found: string[] = [];
  while (pending.length > 0) {
    const current = pending.pop()!;
    for (const entry of fs.readdirSync(current, { withFileTypes: true })) {
      const candidate = path.join(current, entry.name);
      if (entry.isDirectory()) pending.push(candidate);
      if (entry.isFile()) found.push(candidate);
    }
  }
  return found.sort();
}

function dispatchDbs(root: string): string[] {
  return filesUnder(root).filter((candidate) => candidate.endsWith('dispatch.db'));
}

function allFiles(root: string): string[] {
  return filesUnder(root).map((candidate) => path.relative(root, candidate));
}

function rows(database: string): DispatchRow[] {
  return withSqlite(database, true, (connection) => connection.prepare(
    'SELECT run_id, thread_id, status, lease_until, sandbox, request FROM runtime_dispatch ORDER BY created_at',
  ).all() as unknown as DispatchRow[]);
}

function committedMessages(database: string, threadId: string): any[] {
  return withSqlite(database, true, (connection) => (
    connection.prepare(
      'SELECT data FROM runtime_message WHERE thread_id = ? ORDER BY id',
    ).all(threadId) as unknown as Array<{ data: string }>
  ).map((row) => JSON.parse(row.data)));
}

function withSqlite<T>(
  database: string,
  readOnly: boolean,
  operation: (connection: DatabaseSync) => T,
): T {
  // Persistence-inspection cause graph:
  // C1 external sqlite3 CLI installed -> legacy fixture can inspect/mutate;
  // C2 Node runtime provides SQLite -> portable fixture can inspect/mutate.
  // C2 is sufficient and removes C1 from the E2E environment contract. A bounded
  // busy timeout still serializes with the real server's concurrent transaction.
  //
  // | Rule | sqlite3 CLI | node:sqlite | Result |
  // |---|---|---|---|
  // | Q1 | F | T | inspect/recover |
  // | Q2 | T | T | inspect/recover without subprocess |
  const connection = new DatabaseSync(database, { readOnly });
  try {
    connection.exec('PRAGMA busy_timeout = 10000');
    return operation(connection);
  } finally {
    connection.close();
  }
}

function boundChild(root: string): {
  database: string;
  parent: DispatchRow;
  child: DispatchRow;
} {
  const observed: DispatchRow[] = [];
  for (const database of dispatchDbs(root)) {
    const current = rows(database);
    observed.push(...current);
    if (current.length >= 2) {
      const parent = current[0];
      const child = current.find((row) => row.thread_id !== parent.thread_id);
      if (child?.status === 'running' && parent.sandbox) return { database, parent, child };
    }
  }
  throw new Error(
    `crashed child was not durably sandbox-bound; rows=${JSON.stringify(observed)} files=${JSON.stringify(allFiles(root))}`,
  );
}

async function waitForChildInference(upstream: { received: number }, timeoutMs = 20_000): Promise<void> {
  const deadline = Date.now() + timeoutMs;
  while (upstream.received < 2) {
    if (Date.now() > deadline) throw new Error(`child inference never reached the real HTTP upstream`);
    await sleep(20);
  }
}

async function waitForMoreInference(
  upstream: { received: number },
  previous: number,
  timeoutMs = 20_000,
): Promise<void> {
  const deadline = Date.now() + timeoutMs;
  while (upstream.received <= previous) {
    if (Date.now() > deadline) throw new Error('recovered run never re-entered the real HTTP model');
    await sleep(20);
  }
}

async function waitForReply(thread: string, timeoutMs = 30_000): Promise<string> {
  const deadline = Date.now() + timeoutMs;
  let observed: any[] = [];
  for (;;) {
    const response = await fetch(`${BASE}/v1/durable/threads/${thread}/messages`);
    if (response.status === 200) {
      const messages = ((await response.json()) as any).messages ?? [];
      observed = messages;
      const reply = messages.find(
        (message: any) =>
          message.role === 'Assistant' && String(message.text ?? '').includes('delegate said: researched: 42'),
      );
      if (reply) return reply.text;
    }
    if (Date.now() > deadline) {
      throw new Error(`recovered parent did not commit its child result; messages=${JSON.stringify(observed)}`);
    }
    await sleep(100);
  }
}

async function main(): Promise<void> {
  console.log('[child-recovery] preparing fixtures');
  const storage = mkdtempSync(path.join(tmpdir(), 'awaken-child-sandbox-recovery-'));
  const metricBodies: string[] = [];
  const metricReceiver = http.createServer((request, response) => {
    const chunks: Buffer[] = [];
    request.on('data', (chunk) => chunks.push(chunk));
    request.on('end', () => {
      metricBodies.push(Buffer.concat(chunks).toString('latin1'));
      response.writeHead(200, { 'content-type': 'application/x-protobuf' });
      response.end();
    });
  });
  await new Promise<void>((resolve) => metricReceiver.listen(0, '127.0.0.1', resolve));
  const metricAddress = metricReceiver.address();
  assert.ok(metricAddress && typeof metricAddress !== 'string');
  // Delay every real Anthropic response to leave a deterministic crash window
  // after the child is durably created/bound but before its inference completes.
  const upstream = await startFakeAnthropic(FAKE_KEY, { behavior: 'delegating', delayMs: 1_500 });
  const environment = realServerEnv('delegating', upstream, {
    mode: 'delegate',
    extraEnv: {
      SESSION_DEPLOYMENT_INGRESS: 'durable',
      SESSION_DEPLOYMENT_STORAGE_DIR: storage,
      OTEL_EXPORTER_OTLP_ENDPOINT: `http://127.0.0.1:${metricAddress.port}`,
      OTEL_EXPORTER_OTLP_PROTOCOL: 'http/protobuf',
      OTEL_METRIC_EXPORT_INTERVAL: '250',
    },
  });
  let server = spawnServer('delegate', PORT, environment).server;
  try {
    console.log('[child-recovery] waiting for initial server');
    await waitForPort(PORT);
    const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: BASE });
    const session = await client.beta.sessions.create({
      agent: 'assistant',
      environment_id: 'env_local',
      betas: BETAS,
    });
    const submitted = await fetch(`${BASE}/v1/durable/threads/${session.id}/submit_background`, {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ text: 'research the answer through a durable child' }),
    });
    assert.equal(submitted.status, 200, `parent background run accepted: ${await submitted.text()}`);

    console.log('[child-recovery] waiting for bound child inference');
    await waitForChildInference(upstream);

    // Simulate a hard worker crash: no graceful settle/commit hooks run.
    const crashed = new Promise<void>((resolve) => server.once('exit', () => resolve()));
    server.kill('SIGKILL');
    await crashed;
    console.log('[child-recovery] initial server crashed');

    const before = boundChild(storage);
    const childRunId = before.child.run_id;
    const childThreadId = before.child.thread_id;
    const sessionSandbox = before.parent.sandbox;
    assert.ok(sessionSandbox, 'the parent session sandbox was durably bound before child execution');
    // Native children deliberately share the parent session sandbox, so the child
    // dispatch routes through session_thread_id instead of owning a second handle.
    assert.equal(before.child.sandbox, null);

    // Deterministically advance the lease boundary without making the e2e sleep
    // 30 seconds. This mutates only the throwaway dispatch DB created above.
    withSqlite(before.database, false, (connection) => {
      connection.exec("UPDATE runtime_dispatch SET lease_until = 0 WHERE status = 'running'");
    });

    const receivedBeforeRestart = upstream.received;
    // Recovery routing cause/effect graph: C1=Run thread equals its Session
    // thread (root); C2=Run thread is a first-class child while
    // session_thread_id names its parent; C3=the child carries its own immutable
    // Agent snapshot; C4=the parent owns the durable Sandbox. E1=root resolves
    // normally; E2=parent projection/environment is adopted; E3=child attempt
    // executor is rebuilt from C3; C5=parent and child share the SessionCtx's
    // single hydrated commit/read boundary. E4=child commits on its own thread
    // and the waiting parent observes it without rebinding the parent Agent.
    // C1/C2 are exclusive. Decision table:
    // | Rule | C1 | C2 | C3 | C4 | C5 | Effect       |
    // | R1   | T  | F  | -  | -  | T  | E1           |
    // | R2   | F  | T  | T  | T  | T  | E2,E3,E4     |
    // | R3   | F  | T  | F  | *  | *  | fail closed  |
    // | R4   | F  | T  | T  | F  | *  | retry; never rebind |
    // | R5   | F  | T  | T  | T  | F  | forbidden parallel projection |
    server = spawnServer('delegate', PORT, environment).server;
    console.log('[child-recovery] waiting for replacement server');
    await waitForPort(PORT);
    await waitForMoreInference(upstream, receivedBeforeRestart);
    console.log('[child-recovery] replacement re-entered inference');
    const duringRecovery = boundChild(storage);
    const recoveringRows = rows(duringRecovery.database);
    assert.equal(recoveringRows.length, 2, 'recovery retained exactly the parent and one child');
    assert.equal(
      new Set(recoveringRows.map((row) => row.run_id)).size,
      2,
      'recovery did not enqueue a duplicate child identity',
    );
    assert.equal(duringRecovery.child.run_id, childRunId, 'recovery reused the stable child run id');
    assert.equal(
      duringRecovery.parent.sandbox,
      sessionSandbox,
      'replacement process adopted the same durable session sandbox handle',
    );
    let reply: string;
    try {
      reply = await waitForReply(session.id);
    } catch (error) {
      const childMessages = committedMessages(
        sqliteDatabaseForThread(storage, childThreadId, 'runtime_message'),
        childThreadId,
      );
      throw new Error(
        `${error}; dispatch=${JSON.stringify(rows(before.database))}; child=${JSON.stringify(childMessages)}`,
      );
    }
    assert.ok(reply.includes('delegate said: researched: 42'));
    console.log('[child-recovery] parent result committed');

    // The public session registry is process-local. The parent's durable commit
    // boundary nevertheless contains the ordinary child thread as committed
    // truth, so inspect that throwaway SQLite boundary directly and prove the
    // replacement neither duplicated its seed nor lost its terminal result.
    const childMessages = committedMessages(
      // Commit-boundary decision table: a unique runtime_message DB containing
      // the exact child thread is authoritative; absent-table/nonmatching DBs
      // are ignored, while zero or multiple matches fail closed. Discovery by
      // content keeps the product's filename codec as the sole implementation.
      sqliteDatabaseForThread(storage, childThreadId, 'runtime_message'),
      childThreadId,
    );
    assert.equal(
      childMessages.filter((message: any) => message.role === 'User').length,
      1,
      'stable child recovery did not duplicate its seed',
    );
    assert.equal(
      childMessages.find((message: any) => message.role === 'User')?.id,
      `${childRunId}-input`,
      'the child seed identity is derived from its durable Run id across processes',
    );
    assert.equal(
      childMessages.filter(
        (message: any) =>
          message.role === 'Assistant' &&
          (message.content ?? []).some(
            (block: any) => block.type === 'text' && String(block.text ?? '').includes('researched: 42'),
          ),
      ).length,
      1,
      'stable child recovery committed one terminal result',
    );
    assert.equal(rows(before.database).length, 0, 'parent and child dispatches settled exactly once');
    const metricDeadline = Date.now() + 10_000;
    while (
      !metricBodies.some((body) => body.includes('awaken.dispatch.runs.recovered')) &&
      Date.now() <= metricDeadline
    ) {
      await sleep(50);
    }
    assert.ok(
      metricBodies.some((body) => body.includes('awaken.dispatch.runs.recovered')),
      'replacement worker exported the expired-lease recovery metric',
    );

    console.log(
      'DURABLE CHILD/SANDBOX TS E2E PASS: hard crash recovered one stable child, adopted its session sandbox, resumed the parent and avoided duplicate execution.',
    );
  } finally {
    console.log('[child-recovery] cleaning up');
    await stopServer(server).catch(() => {});
    upstream.close();
    metricReceiver.close();
    if (process.env.E2E_KEEP_ARTIFACTS === '1') {
      console.error(`[child-recovery] retained diagnostics at ${storage}`);
    } else {
      fs.rmSync(storage, { recursive: true, force: true });
    }
  }
}

main().catch((error) => {
  console.error('DURABLE CHILD/SANDBOX TS E2E FAIL:', error);
  process.exitCode = 1;
});
