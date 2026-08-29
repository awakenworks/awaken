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
import {
  FAKE_KEY,
  realServerEnv,
  spawnServer,
  stopServer,
  waitForPort,
  waitForValue,
} from './harness.mjs';
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

type BoundChild = {
  database: string;
  parent: DispatchRow;
  child: DispatchRow;
};

type BoundChildObservation = {
  bound?: BoundChild;
  observed: DispatchRow[];
  files: string[];
};

function inspectBoundChild(root: string): BoundChildObservation {
  const observed: DispatchRow[] = [];
  for (const database of dispatchDbs(root)) {
    const current = rows(database);
    observed.push(...current);
    if (current.length >= 2) {
      const parent = current[0];
      const child = current.find((row) => row.thread_id !== parent.thread_id);
      if (child?.status === 'running' && parent.sandbox) {
        return { bound: { database, parent, child }, observed, files: allFiles(root) };
      }
    }
  }
  return { observed, files: allFiles(root) };
}

function boundChild(root: string): BoundChild {
  const snapshot = inspectBoundChild(root);
  if (snapshot.bound) return snapshot.bound;
  throw new Error(
    `crashed child was not durably sandbox-bound; rows=${JSON.stringify(snapshot.observed)} files=${JSON.stringify(snapshot.files)}`,
  );
}

async function waitForBoundChild(root: string, timeoutMs = 20_000): Promise<BoundChild> {
  const snapshot = await waitForValue(
    async () => inspectBoundChild(root),
    (observed: BoundChildObservation) => observed.bound !== undefined,
    'the child dispatch to become durably running with its parent sandbox bound',
    { timeoutMs, pollMs: 20 },
  ) as BoundChildObservation;
  return snapshot.bound!;
}

async function waitForMoreInference(
  upstream: { received: number },
  previous: number,
  timeoutMs = 20_000,
): Promise<void> {
  await waitForValue(
    async () => upstream.received,
    (received: number) => received > previous,
    'the recovered Run to re-enter the real HTTP model',
    { timeoutMs, pollMs: 20 },
  );
}

async function waitForReply(thread: string, timeoutMs = 30_000): Promise<string> {
  const observed = await waitForValue(
    async () => {
      const response = await fetch(`${BASE}/v1/durable/threads/${thread}/messages`);
      const messages = response.status === 200
        ? ((await response.json()) as any).messages ?? []
        : [];
      const reply = messages.find(
        (message: any) =>
          message.role === 'Assistant'
          && message.text === 'coordination completed from child report',
      );
      return { messages, reply: reply?.text as string | undefined };
    },
    (value: { reply?: string }) => value.reply !== undefined,
    'the recovered parent to commit its child result',
    { timeoutMs, pollMs: 100 },
  ) as { messages: any[]; reply: string };
  return observed.reply;
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
    // Root-ingress decision row I1: this is a Managed Session aggregate, so the
    // official Session event command owns admission and its Run reservation.
    // Generic durable submit is reserved for ordinary Runtime Threads.
    const submitted = await client.beta.sessions.events.send(session.id, {
      events: [{
        type: 'user.message',
        content: [{ type: 'text', text: 'research the answer through a durable child' }],
      }],
      betas: BETAS,
    });
    assert.equal(typeof submitted.data[0]?.id, 'string', 'I1 Session event batch is durably accepted');

    // Crash-window cause/effect graph: C1=the root may consume multiple model
    // Steps before delegation; C2=a distinct child dispatch exists and is
    // running; C3=the parent sandbox handle is durable. Effects: E1=C1 alone
    // keeps polling; E2=C2+C3 permits the crash. The fake upstream arrival count
    // is not a child identity and therefore cannot satisfy E2.
    //
    // | Rule | child running | parent sandbox | Effect |
    // | B1   | no            | any            | wait; do not crash |
    // | B2   | yes           | yes            | crash exact durable child |
    // Constraints/invariant: only the persisted child dispatch plus parent
    // sandbox binding identifies the crash window; provider arrival counts do not.
    console.log('[child-recovery] waiting for durable child dispatch');
    await waitForBoundChild(storage);

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
    // Seed-identity decision table: S1=the pre-crash durable child request owns
    // one frozen User seed with the coordination Message family; S2=recovery
    // completes. S1+S2 must commit that exact Message id once. Deriving the id
    // from the Run in this E2E would duplicate the production fingerprint owner.
    const frozenChildRequest = JSON.parse(before.child.request);
    const childSeedBefore = frozenChildRequest.activation?.input?.find(
      (message: any) => message.role === 'User',
    );
    assert.match(childSeedBefore?.id ?? '', /^coord-input-/u, 'S1 exact frozen child seed family');

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
    // and the waiting parent observes it through the fixture's canonical
    // `coordination completed from child report` acknowledgement without
    // rebinding the parent Agent.
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
    assert.equal(reply, 'coordination completed from child report');
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
      childSeedBefore.id,
      'S2 recovery preserves the exact committed child seed id across processes',
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
    // Queue-settlement decision table: C1=the recovered parent reply is committed;
    // C2=its Worker may still own the committed-but-not-yet-settled dispatch;
    // C3=both exact parent/child rows have settled. Effects: E1=C1+C2=>keep the
    // latest rows and retry; E2=C1+C3=>observe the empty authority; a deadline
    // fails with those latest rows. Constraints: K1 Run commit causally precedes
    // queue settle but is not atomic with it; K2 this read-only observer uses the
    // same exact dispatch DB and cannot drive settlement. Rules Q1=C1+C2=>E1;
    // Q2=C1+C3=>E2.
    const settledDispatches = await waitForValue(
      async () => rows(before.database),
      (dispatches: DispatchRow[]) => dispatches.length === 0,
      'the recovered parent and child dispatches to settle exactly once',
      { timeoutMs: 10_000, pollMs: 20 },
    ) as DispatchRow[];
    assert.equal(settledDispatches.length, 0, 'parent and child dispatches settled exactly once');
    await waitForValue(
      async () => metricBodies.some((body) => body.includes('awaken.dispatch.runs.recovered')),
      (observed: boolean) => observed,
      'the replacement Worker to export its expired-lease recovery metric',
      { timeoutMs: 10_000, pollMs: 50 },
    );
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
