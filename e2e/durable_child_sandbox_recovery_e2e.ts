// Cross-process durable child + sandbox recovery over the real server binary.
//
// The parent delegates to a first-class child Run. We crash while the child is
// executing with a bound sandbox, expire the abandoned SQLite lease (equivalent
// to passage of lease time), restart on the same durable directory, and assert:
// the stable child identity is reused, its sandbox handle is adopted unchanged,
// the result resumes the parent, and no duplicate child relationship appears.

import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import fs, { mkdtempSync } from 'node:fs';
import http from 'node:http';
import { tmpdir } from 'node:os';
import path from 'node:path';
import Anthropic from '@anthropic-ai/sdk';
import {
  FAKE_KEY,
  realServerEnv,
  spawnServer,
  stopServer,
  waitForPort,
} from './harness.mjs';
import { startFakeAnthropic } from './fixtures/fake_anthropic_fixture.mjs';

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

function dispatchDbs(root: string): string[] {
  const pending = [root];
  const found: string[] = [];
  while (pending.length > 0) {
    const current = pending.pop()!;
    for (const entry of fs.readdirSync(current, { withFileTypes: true })) {
      const candidate = path.join(current, entry.name);
      if (entry.isDirectory()) pending.push(candidate);
      if (entry.isFile() && entry.name.endsWith('dispatch.db')) found.push(candidate);
    }
  }
  return found;
}

function allFiles(root: string): string[] {
  const pending = [root];
  const found: string[] = [];
  while (pending.length > 0) {
    const current = pending.pop()!;
    for (const entry of fs.readdirSync(current, { withFileTypes: true })) {
      const candidate = path.join(current, entry.name);
      if (entry.isDirectory()) pending.push(candidate);
      if (entry.isFile()) found.push(path.relative(root, candidate));
    }
  }
  return found.sort();
}

function rows(database: string): DispatchRow[] {
  const output = execFileSync(
    'sqlite3',
    [
      '-cmd',
      '.timeout 2000',
      '-json',
      database,
      'SELECT run_id, thread_id, status, lease_until, sandbox, request FROM runtime_dispatch ORDER BY created_at',
    ],
    { encoding: 'utf8' },
  ).trim();
  return output ? (JSON.parse(output) as DispatchRow[]) : [];
}

function committedMessages(database: string, threadId: string): any[] {
  const escapedThread = threadId.replaceAll("'", "''");
  const output = execFileSync(
    'sqlite3',
    [
      '-cmd',
      '.timeout 2000',
      '-json',
      database,
      `SELECT data FROM runtime_message WHERE thread_id = '${escapedThread}' ORDER BY id`,
    ],
    { encoding: 'utf8' },
  ).trim();
  if (!output) return [];
  return (JSON.parse(output) as Array<{ data: string }>).map((row) => JSON.parse(row.data));
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
      AWAKEN_INGRESS: 'durable',
      AWAKEN_STORAGE_DIR: storage,
      OTEL_EXPORTER_OTLP_ENDPOINT: `http://127.0.0.1:${metricAddress.port}`,
      OTEL_EXPORTER_OTLP_PROTOCOL: 'http/protobuf',
      OTEL_METRIC_EXPORT_INTERVAL: '250',
    },
  });
  let server = spawnServer('delegate', PORT, environment).server;
  try {
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

    await waitForChildInference(upstream);

    // Simulate a hard worker crash: no graceful settle/commit hooks run.
    const crashed = new Promise<void>((resolve) => server.once('exit', () => resolve()));
    server.kill('SIGKILL');
    await crashed;

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
    execFileSync('sqlite3', [
      before.database,
      "UPDATE runtime_dispatch SET lease_until = 0 WHERE status = 'running'",
    ]);

    const receivedBeforeRestart = upstream.received;
    server = spawnServer('delegate', PORT, environment).server;
    await waitForPort(PORT);
    await waitForMoreInference(upstream, receivedBeforeRestart);
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
      throw new Error(
        `${error}; dispatch=${JSON.stringify(rows(before.database))}`,
      );
    }
    assert.ok(reply.includes('delegate said: researched: 42'));

    // The public session registry is process-local. The parent's durable commit
    // boundary nevertheless contains the ordinary child thread as committed
    // truth, so inspect that throwaway SQLite boundary directly and prove the
    // replacement neither duplicated its seed nor lost its terminal result.
    const childMessages = committedMessages(path.join(storage, `${session.id}.db`), childThreadId);
    assert.equal(
      childMessages.filter((message: any) => message.role === 'User').length,
      1,
      'stable child recovery did not duplicate its seed',
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
    await stopServer(server).catch(() => {});
    upstream.close();
    metricReceiver.close();
    fs.rmSync(storage, { recursive: true, force: true });
  }
}

main().catch((error) => {
  console.error('DURABLE CHILD/SANDBOX TS E2E FAIL:', error);
  process.exitCode = 1;
});
