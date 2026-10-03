// Cross-process durable child + sandbox recovery over the real server binary.
//
// The parent delegates to a first-class child Run. A test-owned SQLite trigger
// holds its dispatch settlement after the child model response and terminal
// commit; a hard crash then exercises the ordinary committed-terminal repair.
// A separate in-flight crash checks that an unacknowledged model operation is
// retained and never replayed from lease expiry or a fake provider-side count.

import assert from 'node:assert/strict';
import fs, { mkdtempSync } from 'node:fs';
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
  lease_owner: string | null;
  lease_epoch: number;
  active_attempt_owner: string | null;
  active_attempt_epoch: number | null;
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
    `SELECT run_id, thread_id, status, lease_until, lease_owner, lease_epoch,
            active_attempt_owner, active_attempt_epoch, sandbox, request
       FROM runtime_dispatch ORDER BY created_at`,
  ).all() as unknown as DispatchRow[]);
}

function dispatchCoordinates(dispatches: DispatchRow[]): Array<Omit<DispatchRow, 'request' | 'sandbox'> & { sandbox_bound: boolean }> {
  return dispatches.map(({ request: _request, sandbox, ...row }) => ({
    ...row,
    sandbox_bound: sandbox !== null,
  }));
}

function committedMessages(database: string, threadId: string): any[] {
  return withSqlite(database, true, (connection) => (
    connection.prepare(
      'SELECT data FROM runtime_message WHERE thread_id = ? ORDER BY id',
    ).all(threadId) as unknown as Array<{ data: string }>
  ).map((row) => JSON.parse(row.data)));
}

function committedModelOperations(database: string, threadId: string, runId: string): any[] {
  return withSqlite(database, true, (connection) => (
    connection.prepare(
      'SELECT data FROM runtime_state_command WHERE thread_id = ? ORDER BY id',
    ).all(threadId) as unknown as Array<{ data: string }>
  )).map((row) => JSON.parse(row.data))
    .filter((command) => command.run_id === runId && command.key === 'runtime.model_operation.v1')
    .map((command) => command.action?.Set);
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
      // Durable identity, not insertion order, distinguishes the self-affine
      // Session root from a child. Recovery may delete/reinsert a dispatch and
      // legitimately reverse created_at ordering.
      const parent = current.find((row) => {
        const request = JSON.parse(row.request);
        return request.session_thread_id === row.thread_id;
      });
      const child = parent
        ? current.find((row) => row.thread_id !== parent.thread_id)
        : undefined;
      if (parent && child?.status === 'running' && parent.sandbox) {
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
    `crashed child was not durably sandbox-bound; rows=${JSON.stringify(dispatchCoordinates(snapshot.observed))} files=${JSON.stringify(snapshot.files)}`,
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

function requestsFor(
  requests: Array<{ lastUserText?: string }>,
  prompt: string,
): number {
  return requests.filter((request) => request.lastUserText === prompt).length;
}

function isResearchedAnswer(message: any): boolean {
  return message.role === 'Assistant'
    && (message.content ?? []).some(
      (block: any) => block.type === 'text' && String(block.text ?? '').includes('researched: 42'),
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
  // Delay the real HTTP response so the test can observe both committed and
  // in-flight child boundaries without guessing from a wall-clock sleep.
  const upstream = await startFakeAnthropic(FAKE_KEY, { behavior: 'delegating', delayMs: 1_500 });
  const environment = realServerEnv('delegating', upstream, {
    mode: 'delegate',
    extraEnv: {
      SESSION_DEPLOYMENT_SANDBOX_TIER: 'namespace',
      SESSION_DEPLOYMENT_INGRESS: 'durable',
      SESSION_DEPLOYMENT_STORAGE_DIR: storage,
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
    // Settlement-gap decision table: C1 a child model response and Run terminal
    // commit are complete; C2 only the child Dispatch Done is rejected. C1+C2
    // => the committed child result remains authoritative while its exact
    // dispatch row stays repairable. An unreturned model response cannot satisfy
    // C1 and is covered by the in-flight crash rule below.
    const [dispatchDatabase] = await waitForValue(
      async () => dispatchDbs(storage),
      (databases: string[]) => databases.length === 1,
      'one durable Dispatch authority for the Session',
      { timeoutMs: 5_000, pollMs: 20 },
    ) as string[];
    withSqlite(dispatchDatabase, false, (connection) => connection.exec(`
      CREATE TRIGGER hold_child_terminal_settle BEFORE DELETE ON runtime_dispatch
      WHEN OLD.thread_id <> json_extract(OLD.request, '$.session_thread_id')
      BEGIN SELECT RAISE(ABORT, 'test-owned child settlement gap'); END;
    `));
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
    assert.equal(typeof submitted.data?.[0]?.id, 'string', 'I1 Session event batch is durably accepted');

    // Crash-window decision table: C1 the exact child dispatch and parent
    // sandbox are durable; C2 the child has committed Ended and its model result;
    // C3 its physical attempt slot is empty; C4 Dispatch Done is held by the
    // test trigger. B1 C1+!C2 or !C3 => keep waiting, never infer completion
    // from the provider's arrival count. B2 C1+C2+C3+C4 => crash after the
    // committed result and before dispatch settlement. Recovery may repair the
    // one exact row without re-entering the provider.
    console.log('[child-recovery] waiting for durable child dispatch');
    const before = await waitForBoundChild(storage);
    const childRunId = before.child.run_id;
    const childThreadId = before.child.thread_id;
    const sessionSandbox = before.parent.sandbox;
    assert.ok(sessionSandbox, 'the parent session sandbox was durably bound before child execution');
    assert.equal(before.child.sandbox, null, 'Native child shares the parent Session sandbox');
    const frozenChildRequest = JSON.parse(before.child.request);
    const childSeedBefore = frozenChildRequest.activation?.input?.find(
      (message: any) => message.role === 'User',
    );
    assert.match(childSeedBefore?.id ?? '', /^coord-input-/u, 'S1 exact frozen child seed family');
    const childMessageDatabase = await waitForValue(
      async () => {
        try {
          return sqliteDatabaseForThread(storage, childThreadId, 'runtime_message');
        } catch (error) {
          if (String(error).includes('matches=[]')) return undefined;
          throw error;
        }
      },
      (database: string | undefined) => database !== undefined,
      'the exact child Thread to commit its first Message',
      { timeoutMs: 30_000, pollMs: 20 },
    ) as string;
    await waitForValue(
      async () => {
        const child = rows(before.database).find((row) => row.run_id === childRunId);
        const phase = withSqlite(childMessageDatabase, true, (connection) => (
          connection.prepare('SELECT phase FROM runtime_run_record WHERE run_id = ?')
            .get(childRunId) as { phase?: string } | undefined
        ))?.phase;
        const result = committedMessages(childMessageDatabase, childThreadId)
          .some(isResearchedAnswer);
        return { child, terminal: String(phase).includes('Ended'), result };
      },
      (observed: { child?: DispatchRow; terminal: boolean; result: boolean }) =>
        observed.child?.active_attempt_epoch === null && observed.terminal && observed.result,
      'the committed child result and empty physical slot behind held Dispatch Done',
      { timeoutMs: 30_000, pollMs: 20 },
    );
    assert.equal(requestsFor(upstream.receivedRequests, 'do the research'), 1,
      'one child model request produced the committed result');

    // The terminal Run is committed while the exact Dispatch row remains.
    // SIGKILL cannot manufacture a missing model receipt or duplicate its seed.
    const crashed = new Promise<void>((resolve) => server.once('exit', () => resolve()));
    server.kill('SIGKILL');
    await crashed;
    console.log('[child-recovery] initial server crashed');

    // Exact repair rule P1: committed Ended + empty physical slot + expired
    // claim allows the existing terminal reconciler to apply Done. Drop the
    // test-owned settle fault before restart; otherwise the scanner correctly
    // retains its failed claim until another lease expires.
    withSqlite(before.database, false, (connection) => {
      connection.exec(`
        DROP TRIGGER hold_child_terminal_settle;
        UPDATE runtime_dispatch SET lease_until = 0 WHERE status = 'running';
      `);
    });
    const childReceivedBeforeRestart = requestsFor(
      upstream.receivedRequests,
      'do the research',
    );
    // Recovery routing rule R1: the child retains its own Run/thread identity,
    // the parent retains the Session sandbox, and the committed child result
    // resumes the waiting parent through the one hydrated commit/read boundary.
    // R2: if the model response were unknown, the same root/child affinity may
    // not authorize replay; that negative partition is exercised below.
    server = spawnServer('delegate', PORT, environment).server;
    console.log('[child-recovery] waiting for replacement server');
    await waitForPort(PORT);
    let reply: string;
    try {
      reply = await waitForReply(session.id);
    } catch (error) {
      const childMessages = committedMessages(
        sqliteDatabaseForThread(storage, childThreadId, 'runtime_message'),
        childThreadId,
      );
      throw new Error(
        `${error}; dispatch=${JSON.stringify(dispatchCoordinates(rows(before.database)))}; child=${JSON.stringify(childMessages)}; provider=${JSON.stringify({ received: upstream.receivedRequests.map((request: { lastUserText?: string }) => request.lastUserText), terminal: upstream.requests.map((request: { lastUserText?: string }) => request.lastUserText) })}`,
      );
    }
    assert.equal(reply, 'coordination completed from child report');
    console.log('[child-recovery] parent result committed');
    assert.equal(requestsFor(upstream.receivedRequests, 'do the research'), childReceivedBeforeRestart,
      'committed child model result was not replayed after restart');
    const realizationFiles = filesUnder(path.join(storage, 'sandboxes'))
      .filter((file) => file.endsWith('.awaken-realization.json'));
    assert.equal(realizationFiles.length, 1, 'one Session sandbox realization survives restart');
    const realization = JSON.parse(fs.readFileSync(realizationFiles[0], 'utf8'));
    assert.equal(realization.physical_incarnation, JSON.parse(sessionSandbox).payload.physical_incarnation,
      'the replacement retained the exact physical Session sandbox incarnation');

    // The public session registry is process-local. The parent's durable commit
    // boundary nevertheless contains the ordinary child thread as committed
    // truth, so inspect that throwaway SQLite boundary directly and prove the
    // replacement neither duplicated its seed nor lost its terminal result.
    const childMessages = committedMessages(childMessageDatabase, childThreadId);
    const committedChildRun = withSqlite(childMessageDatabase, true, (connection) => (
      connection.prepare('SELECT thread_id, phase FROM runtime_run_record WHERE run_id = ?')
        .get(childRunId) as { thread_id?: string; phase?: string } | undefined
    ));
    assert.equal(committedChildRun?.thread_id, childThreadId,
      'the recovered committed Run retains the exact child identity');
    assert.match(String(committedChildRun?.phase), /Ended/u,
      'the child terminal Run fact survived the process crash');
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
      childMessages.filter(isResearchedAnswer).length,
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
    // In-flight crash decision table. C1 a distinct child request reached the
    // HTTP provider, but its committed ModelOperation is Entered/Unknown; C2
    // the Worker dies before a complete response; C3 a replacement reclaims
    // the expired Dispatch lease. C1+C2+C3 => the old physical slot remains
    // occupied, no second model request or child result is manufactured.
    const unknownSession = await client.beta.sessions.create({
      agent: 'assistant', environment_id: 'env_local', betas: BETAS,
    });
    const priorChildArrivals = requestsFor(upstream.receivedRequests, 'do the research');
    await client.beta.sessions.events.send(unknownSession.id, {
      events: [{
        type: 'user.message',
        content: [{ type: 'text', text: 'research through another durable child' }],
      }],
      betas: BETAS,
    });
    await waitForBoundChild(storage);
    await waitForValue(
      async () => requestsFor(upstream.receivedRequests, 'do the research'),
      (count: number) => count === priorChildArrivals + 1,
      'the distinct child provider request to arrive before the hard crash',
      { timeoutMs: 10_000, pollMs: 20 },
    );
    const unknownBefore = boundChild(storage);
    assert.notEqual(unknownBefore.child.active_attempt_epoch, null,
      'C1 the in-flight child owns its physical attempt before SIGKILL');
    const unknownStateDatabase = sqliteDatabaseForThread(
      storage, unknownBefore.child.thread_id, 'runtime_state_command',
    );
    const unknownOperation = committedModelOperations(
      unknownStateDatabase, unknownBefore.child.thread_id, unknownBefore.child.run_id,
    ).at(-1);
    assert.equal(unknownOperation?.phase, 'Entered', 'C1 incomplete model invocation');
    assert.equal(unknownOperation?.execution, 'Unknown', 'C1 has no sealed terminal receipt');
    const unknownCrash = new Promise<void>((resolve) => server.once('exit', () => resolve()));
    server.kill('SIGKILL');
    await unknownCrash;
    withSqlite(unknownBefore.database, false, (connection) => {
      connection.exec("UPDATE runtime_dispatch SET lease_until = 0 WHERE status = 'running'");
    });
    server = spawnServer('delegate', PORT, environment).server;
    await waitForPort(PORT);
    const fencedChild = await waitForValue(
      async () => rows(unknownBefore.database).find(
        (row) => row.run_id === unknownBefore.child.run_id,
      ),
      (row: DispatchRow | undefined) => row !== undefined
        && row.lease_epoch > unknownBefore.child.lease_epoch
        && row.active_attempt_epoch === unknownBefore.child.active_attempt_epoch,
      'the replacement claim to retain the exact unconfirmed child attempt',
      { timeoutMs: 10_000, pollMs: 20 },
    ) as DispatchRow;
    assert.equal(fencedChild.thread_id, unknownBefore.child.thread_id,
      'the unknown child keeps its stable Run/thread identity');
    assert.equal(requestsFor(upstream.receivedRequests, 'do the research'), priorChildArrivals + 1,
      'C3 no second child model request crossed the provider boundary');
    assert.equal(committedModelOperations(
      unknownStateDatabase, unknownBefore.child.thread_id, unknownBefore.child.run_id,
    ).at(-1)?.execution, 'Unknown', 'C3 committed uncertainty remains authoritative');
    const unknownChildMessages = committedMessages(
      sqliteDatabaseForThread(storage, unknownBefore.child.thread_id, 'runtime_message'),
      unknownBefore.child.thread_id,
    );
    assert.equal(unknownChildMessages.filter((message: any) => message.role === 'User').length, 1,
      'C3 replacement did not duplicate the child seed');
    assert.equal(unknownChildMessages.filter(isResearchedAnswer).length, 0,
      'C3 no child result is fabricated from an unknown model operation');
    console.log(
      'DURABLE CHILD/SANDBOX TS E2E PASS: committed child terminal repaired after a settle-gap crash; an in-flight child model request stayed fenced and unknown after SIGKILL.',
    );
  } finally {
    console.log('[child-recovery] cleaning up');
    await stopServer(server).catch(() => {});
    upstream.close();
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
