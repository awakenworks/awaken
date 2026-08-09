// Real-process API coverage for a published Agent whose pinned backend is A2A.
// The fake peer implements only the remote HTTP boundary; every Awaken component
// under test is production code: config projection/publication, session binding,
// durable dispatch, A2aRunExecutor, commit/readback, resume and cancellation.

import assert from 'node:assert/strict';
import fs, { mkdtempSync } from 'node:fs';
import http, { type IncomingMessage, type ServerResponse } from 'node:http';
import { tmpdir } from 'node:os';
import path from 'node:path';
// @ts-ignore -- shared JavaScript harness intentionally serves TS scenarios.
import { spawnServer, stopServer, waitForPort } from './harness.mjs';
import { closeHttpServer } from './http_server.mjs';
// @ts-ignore -- shared JavaScript SQLite fixture intentionally serves TS scenarios.
import { sqliteDatabaseForThread, sqliteExec, sqliteRows, sqliteRun } from './sqlite.mjs';

type SeenMessage = { messageId?: string; contextId?: string; text?: string };

const PORT = Number(process.env.E2E_PORT ?? 39771);
const BASE = `http://127.0.0.1:${PORT}`;
const AGENT = 'remote-root';
const sleep = (ms: number): Promise<void> => new Promise((resolve) => setTimeout(resolve, ms));

function task(id: string, contextId: string, state: string, text?: string): Record<string, unknown> {
  return {
    kind: 'task',
    id,
    contextId,
    status: {
      state,
      ...(text
        ? {
            message: {
              kind: 'message',
              messageId: `reply-${id}`,
              role: 'agent',
              parts: [{ kind: 'text', text }],
            },
          }
        : {}),
    },
  };
}

async function requestBody(request: IncomingMessage): Promise<any> {
  const chunks: Buffer[] = [];
  for await (const chunk of request) chunks.push(Buffer.from(chunk));
  const text = Buffer.concat(chunks).toString('utf8');
  return text ? JSON.parse(text) : {};
}

function json(response: ServerResponse, status: number, body?: unknown): void {
  response.writeHead(status, { 'content-type': 'application/json' });
  response.end(body === undefined ? undefined : JSON.stringify(body));
}

async function startPeer(): Promise<{
  endpoint: string;
  close: () => Promise<void>;
  crashPoll: Promise<void>;
  completeCrash: () => void;
  sent: SeenMessage[];
  reads: string[];
  cancels: string[];
}> {
  const sent: SeenMessage[] = [];
  const reads: string[] = [];
  const cancels: string[] = [];
  let crashComplete = false;
  let signalCrashPoll!: () => void;
  const crashPoll = new Promise<void>((resolve) => {
    signalCrashPoll = resolve;
  });

  const server = http.createServer(async (request, response) => {
    const url = request.url ?? '';
    if (request.method === 'POST' && url === '/v1/a2a/message:send') {
      const body = await requestBody(request);
      const message = body.message ?? {};
      const text = (message.parts ?? []).map((part: any) => String(part.text ?? '')).join('');
      sent.push({ messageId: message.messageId, contextId: message.contextId, text });
      if (text.includes('crash recovery')) {
        json(response, 200, { task: task('crash-task', 'crash-context', 'working') });
      } else if (text.includes('need remote input')) {
        json(response, 200, {
          task: task('input-task', 'input-context', 'input-required', 'which file?'),
        });
      } else if (message.contextId === 'input-context') {
        json(response, 200, {
          task: task('input-finished', 'input-context', 'completed', 'REMOTE-RESUME-DONE'),
        });
      } else if (text.includes('cancel remote task')) {
        json(response, 200, {
          task: task('cancel-task', 'cancel-context', 'input-required', 'approve?'),
        });
      } else if (text.includes('active remote cancel')) {
        json(response, 200, {
          task: task('active-cancel-task', 'active-cancel-context', 'working'),
        });
      } else if (text.includes('poll remote failure')) {
        json(response, 200, {
          task: task('poll-failure-task', 'poll-failure-context', 'working'),
        });
      } else if (text.includes('resume remote failure')) {
        json(response, 200, {
          task: task('resume-failure-task', 'resume-failure-context', 'input-required', 'continue?'),
        });
      } else if (message.contextId === 'resume-failure-context') {
        json(response, 503, { error: { message: 'resume transport unavailable' } });
      } else if (text.includes('cancel terminal remote')) {
        json(response, 200, {
          task: task('cancel-terminal-task', 'cancel-terminal-context', 'input-required', 'continue?'),
        });
      } else if (text.includes('need remote auth')) {
        json(response, 200, {
          task: task('auth-task', 'auth-context', 'auth-required', 'supply delegated auth'),
        });
      } else if (message.contextId === 'auth-context') {
        json(response, 200, {
          task: task('auth-finished', 'auth-context', 'completed', 'REMOTE-AUTH-DONE'),
        });
      } else if (text.includes('completed artifact')) {
        const completed = task('artifact-task', 'terminal-context', 'completed');
        completed.artifacts = [{ artifactId: 'artifact-1', parts: [{ kind: 'text', text: 'REMOTE-ARTIFACT-DONE' }] }];
        json(response, 200, { task: completed });
      } else if (text.includes('failed terminal')) {
        json(response, 200, {
          task: task('failed-task', 'terminal-context', 'failed', 'REMOTE-FAILED-DONE'),
        });
      } else if (text.includes('rejected terminal')) {
        const rejected = task('rejected-task', 'terminal-context', 'rejected');
        rejected.history = [{
          kind: 'message',
          messageId: 'rejected-history',
          role: 'agent',
          parts: [{ kind: 'text', text: 'REMOTE-REJECTED-DONE' }],
        }];
        json(response, 200, { task: rejected });
      } else if (text.includes('canceled terminal')) {
        json(response, 200, {
          task: task('canceled-task', 'terminal-context', 'canceled', 'REMOTE-CANCELED-DONE'),
        });
      } else {
        json(response, 400, { error: { message: `unexpected message: ${text}` } });
      }
      return;
    }

    const cancel = url.match(/^\/v1\/a2a\/tasks\/([^/]+):cancel$/);
    if (request.method === 'POST' && cancel) {
      cancels.push(cancel[1]);
      json(response, 204);
      return;
    }

    const get = url.match(/^\/v1\/a2a\/tasks\/([^/]+)$/);
    if (request.method === 'GET' && get) {
      reads.push(get[1]);
      if (get[1] === 'crash-task' && !crashComplete) {
        signalCrashPoll();
        // Deliberately leave this response open. SIGKILL of the first coordinator
        // tears down the socket after its durable task-reference commit.
        request.once('close', () => response.destroy());
        return;
      }
      if (get[1] === 'crash-task') {
        json(response, 200, task('crash-task', 'crash-context', 'completed', 'REMOTE-CRASH-RECOVERED'));
        return;
      }
      if (get[1] === 'cancel-task') {
        json(response, 200, task('cancel-task', 'cancel-context', 'input-required'));
        return;
      }
      if (get[1] === 'active-cancel-task') {
        json(response, 200, task('active-cancel-task', 'active-cancel-context', 'working'));
        return;
      }
      if (get[1] === 'poll-failure-task') {
        json(response, 503, { error: { message: 'poll transport unavailable' } });
        return;
      }
      if (get[1] === 'cancel-terminal-task') {
        json(response, 200, task('cancel-terminal-task', 'cancel-terminal-context', 'completed', 'already done'));
        return;
      }
      json(response, 404, { error: { message: `unknown task ${get[1]}` } });
      return;
    }

    json(response, 404, { error: { message: `unexpected route ${request.method} ${url}` } });
  });
  await new Promise<void>((resolve) => server.listen(0, '127.0.0.1', resolve));
  const address = server.address();
  assert.ok(address && typeof address !== 'string');
  return {
    endpoint: `http://127.0.0.1:${address.port}`,
    close: () => closeHttpServer(server),
    crashPoll,
    completeCrash: () => {
      crashComplete = true;
    },
    sent,
    reads,
    cancels,
  };
}

async function api(method: string, route: string, body?: unknown): Promise<{ status: number; body: any }> {
  const response = await fetch(`${BASE}${route}`, {
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
  const config = {
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
  };
  const stored = await api('PUT', `/v1/config/agents/${AGENT}`, config);
  assert.equal(stored.status, 200, `remote agent stored: ${JSON.stringify(stored.body)}`);
  const published = await api('POST', `/v1/config/agents/${AGENT}/publish`);
  assert.equal(published.status, 200, `remote agent published: ${JSON.stringify(published.body)}`);
  assert.equal(published.body.installed, true);
  const projected = await api('GET', `/v1/config/agents/${AGENT}`);
  assert.equal(projected.body.model.backend_ref, `a2a:${endpoint}`, 'backend_ref round-trips');
}

async function createSession(): Promise<string> {
  const created = await api('POST', '/v1/sessions', {
    agent: AGENT,
    environment_id: 'env_local',
  });
  assert.equal(created.status, 200, `session created: ${JSON.stringify(created.body)}`);
  assert.ok(created.body.id);
  return created.body.id;
}

async function sendText(thread: string, text: string): Promise<void> {
  const response = await api('POST', `/v1/sessions/${thread}/events`, {
    events: [{ type: 'user.message', content: [{ type: 'text', text }] }],
  });
  assert.equal(response.status, 200, `${text}: ${JSON.stringify(response.body)}`);
}

function dispatchDatabases(root: string): string[] {
  const pending = [root];
  const found: string[] = [];
  while (pending.length > 0) {
    const current = pending.pop()!;
    for (const entry of fs.readdirSync(current, { withFileTypes: true })) {
      const candidate = path.join(current, entry.name);
      if (entry.isDirectory()) pending.push(candidate);
      else if (entry.name.endsWith('dispatch.db')) found.push(candidate);
    }
  }
  return found;
}

function committedState(root: string, thread: string): string {
  const database = sqliteDatabaseForThread(root, thread, 'runtime_state_command');
  return sqliteRows(
    database,
    `SELECT data FROM runtime_state_command WHERE thread_id = '${thread.replaceAll("'", "''")}' ORDER BY id`,
  ).map((row: { data: string }) => row.data).join('\n');
}

function rewriteLatestTaskReference(
  root: string,
  thread: string,
  rewrite: (command: any) => void,
): void {
  const database = sqliteDatabaseForThread(root, thread, 'runtime_state_command');
  const row = sqliteRows(
    database,
      `SELECT id || char(9) || data FROM runtime_state_command
       WHERE thread_id = '${thread.replaceAll("'", "''")}'
         AND json_extract(data, '$.key') = '__a2a_task'
         AND json_type(data, '$.action.Set') = 'object'
       ORDER BY id DESC LIMIT 1`,
  )[0] as Record<string, unknown> | undefined;
  const encoded = row ? String(Object.values(row)[0]) : '';
  const separator = encoded.indexOf('\t');
  assert.ok(separator > 0, `durable A2A task reference exists for ${thread}: ${encoded}`);
  const id = Number(encoded.slice(0, separator));
  const command = JSON.parse(encoded.slice(separator + 1));
  rewrite(command);
  const data = JSON.stringify(command).replaceAll("'", "''");
  const changed = sqliteRun(
    database,
    `UPDATE runtime_state_command SET data = '${data}' WHERE id = ${id}`,
  );
  assert.equal(Number(changed.changes), 1, `rewrote exactly one A2A task reference for ${thread}`);
}

async function expectResumeFailure(thread: string, marker: string): Promise<void> {
  const response = await api('POST', `/v1/sessions/${thread}/events`, {
    events: [
      {
        type: 'user.custom_tool_result',
        custom_tool_use_id: 'input-task',
        content: [{ type: 'text', text: marker }],
        is_error: false,
      },
    ],
  });
  assert.equal(response.status, 500, `${marker} failed closed: ${JSON.stringify(response.body)}`);
}

function taskReferenceCleared(root: string, thread: string): boolean {
  const commands = committedState(root, thread)
    .trim()
    .split('\n')
    .filter(Boolean)
    .map((row) => JSON.parse(row))
    .filter((command) => command.scope === 'Run' && command.key === '__a2a_task');
  return commands.length > 0 && commands.at(-1)?.action === 'Remove';
}

function persistedSessions(root: string, threads: string[]): string[] {
  const database = path.join(root, 'sessions.db');
  const placeholders = threads.map(() => '?').join(', ');
  return sqliteRows(
    database,
    `SELECT session_id FROM managed_session WHERE session_id IN (${placeholders}) ORDER BY session_id`,
    ...threads,
  ).map((row: { session_id: string }) => row.session_id);
}

async function waitForMessage(thread: string, marker: string, timeoutMs = 30_000): Promise<any[]> {
  const deadline = Date.now() + timeoutMs;
  let observed: any[] = [];
  while (Date.now() <= deadline) {
    const response = await api('GET', `/v1/durable/threads/${thread}/messages`);
    if (response.status === 200) {
      observed = response.body.messages ?? [];
      if (JSON.stringify(observed).includes(marker)) return observed;
    }
    await sleep(100);
  }
  throw new Error(`timed out waiting for ${marker}; messages=${JSON.stringify(observed)}`);
}

async function waitForAwaiting(thread: string, runId: string): Promise<void> {
  const deadline = Date.now() + 20_000;
  while (Date.now() <= deadline) {
    const response = await api('GET', `/v1/durable/threads/${thread}/dispatches`);
    const row = (response.body.dispatches ?? []).find((entry: any) => entry.run_id === runId);
    if (row?.status === 'Awaiting') return;
    await sleep(50);
  }
  throw new Error(`run ${runId} never reached Awaiting`);
}

async function waitForRemoteCancel(cancels: string[], taskId: string): Promise<void> {
  const deadline = Date.now() + 20_000;
  while (Date.now() <= deadline) {
    if (cancels.includes(taskId)) return;
    await sleep(25);
  }
  throw new Error(`remote cancellation was not delivered to ${taskId}`);
}

async function waitForDispatchGone(thread: string, runId: string): Promise<void> {
  const deadline = Date.now() + 20_000;
  while (Date.now() <= deadline) {
    const response = await api('GET', `/v1/durable/threads/${thread}/dispatches`);
    if (!(response.body.dispatches ?? []).some((entry: any) => entry.run_id === runId)) return;
    await sleep(25);
  }
  throw new Error(`cancelled run ${runId} remained dispatchable`);
}

async function within<T>(promise: Promise<T>, timeoutMs: number, label: string): Promise<T> {
  let timer: NodeJS.Timeout | undefined;
  try {
    return await Promise.race([
      promise,
      new Promise<never>((_, reject) => {
        timer = setTimeout(() => reject(new Error(`timed out waiting for ${label}`)), timeoutMs);
      }),
    ]);
  } finally {
    if (timer) clearTimeout(timer);
  }
}

async function main(): Promise<void> {
  const storage = mkdtempSync(path.join(tmpdir(), 'awaken-remote-attempt-'));
  const peer = await startPeer();
  const environment = {
    SESSION_DEPLOYMENT_INGRESS: 'durable',
    SESSION_DEPLOYMENT_STORAGE_DIR: storage,
  };
  let server = spawnServer('config', PORT, environment).server;
  try {
    await waitForPort(PORT, 180_000, server);
    await publishRemote(peer.endpoint);

    // 1) Crash after task-reference commit but during tasks/get. Replacement must
    // reattach to crash-task from the pinned snapshot and never message:send again.
    const crashThread = await createSession();
    const submitted = await api('POST', `/v1/durable/threads/${crashThread}/submit_background`, {
      agent: AGENT,
      text: 'prove crash recovery',
    });
    assert.equal(submitted.status, 200);
    // Cause graph: valid A2A discriminators -> task reference commit -> tasks/get
    // reaches the peer. A malformed response must fail this edge within 30s instead
    // of leaving the whole stage runner waiting on a promise that can never resolve.
    await within(peer.crashPoll, 30_000, 'the initial remote task poll');
    const killed = new Promise<void>((resolve) => server.once('exit', () => resolve()));
    server.kill('SIGKILL');
    await killed;
    const databases = dispatchDatabases(storage);
    assert.ok(databases.length > 0, 'durable dispatch database exists');
    for (const database of databases) {
      sqliteExec(database, "UPDATE runtime_dispatch SET lease_until = 0 WHERE status = 'running'");
    }
    peer.completeCrash();
    server = spawnServer('config', PORT, environment).server;
    await waitForPort(PORT, 180_000, server);
    await waitForMessage(crashThread, 'REMOTE-CRASH-RECOVERED');
    assert.equal(
      peer.sent.filter((message) => message.text?.includes('crash recovery')).length,
      1,
      'replacement did not resend the initial A2A message',
    );
    assert.deepEqual(
      new Set(peer.reads.filter((id) => id === 'crash-task')),
      new Set(['crash-task']),
      'every poll addressed the committed task identity',
    );

    // Config storage is intentionally process-local in this scenario host. Publish
    // again for new sessions; the recovered run above succeeded before this step,
    // proving recovery consumed its pinned dispatch snapshot rather than reopening config.
    await publishRemote(peer.endpoint);

    // 2) A foreground Managed API turn reaches input-required. A client result
    // resumes the root Run on the exact remote context and commits its terminal reply.
    const inputThread = await createSession();
    const first = await api('POST', `/v1/sessions/${inputThread}/events`, {
      events: [{ type: 'user.message', content: [{ type: 'text', text: 'need remote input' }] }],
    });
    assert.equal(first.status, 200, `remote input turn accepted: ${JSON.stringify(first.body)}`);
    const resumed = await api('POST', `/v1/sessions/${inputThread}/events`, {
      events: [
        {
          type: 'user.custom_tool_result',
          custom_tool_use_id: 'input-task',
          content: [{ type: 'text', text: 'README.md' }],
          is_error: false,
        },
      ],
    });
    assert.equal(resumed.status, 200, `remote input resumed: ${JSON.stringify(resumed.body)}`);
    await waitForMessage(inputThread, 'REMOTE-RESUME-DONE');
    const resumeMessage = peer.sent.find((message) => message.text === 'README.md');
    assert.equal(resumeMessage?.contextId, 'input-context', 'resume retained remote context');
    assert.match(resumeMessage?.messageId ?? '', /^a2a-resume-/, 'resume used stable run/ticket identity');

    // 3) An awaiting background root Run is cancelled through the durable API.
    // The cancellation resolver reconstructs the remote executor without model,
    // credential or sandbox dependencies and addresses the committed task id.
    const cancelThread = await createSession();
    const cancelSubmit = await api('POST', `/v1/durable/threads/${cancelThread}/submit_background`, {
      agent: AGENT,
      text: 'cancel remote task',
    });
    assert.equal(cancelSubmit.status, 200);
    await waitForAwaiting(cancelThread, cancelSubmit.body.run_id);
    // Commit-boundary lookup cause/effect graph: C1=candidate is a SQLite DB
    // containing runtime_state_command; C2=it contains the exact Session thread.
    // Effects: E1 select the one authoritative boundary; E2 reject missing or
    // ambiguous persistence. The product alone owns its filename codec.
    //
    // | Rule | C1 | C2 | effect |
    // | D1 | yes | yes, unique | inspect the committed state |
    // | D2 | yes | no | ignore the candidate |
    // | D3 | no | n/a | ignore the non-commit DB |
    // | D4 | any | zero or multiple | fail closed |
    const stateBeforeCancel = committedState(storage, cancelThread);
    assert.ok(
      stateBeforeCancel.includes('__a2a_task') && stateBeforeCancel.includes('cancel-task'),
      `awaiting run durably committed its remote task reference: ${stateBeforeCancel}`,
    );
    // Remove every resident Session/worker before cancellation. The replacement
    // process must construct the minimal cancellation worker from the pinned
    // dispatch snapshot; it may not reopen the process-local config registry.
    const cancelKilled = new Promise<void>((resolve) => server.once('exit', () => resolve()));
    server.kill('SIGKILL');
    await cancelKilled;
    for (const database of dispatchDatabases(storage)) {
      sqliteExec(database, "UPDATE runtime_dispatch SET lease_until = 0 WHERE status IN ('running', 'awaiting')");
    }
    server = spawnServer('config', PORT, environment).server;
    await waitForPort(PORT, 180_000, server);
    const cancelled = await api('POST', `/v1/durable/threads/${cancelThread}/cancel`, {
      run_id: cancelSubmit.body.run_id,
    });
    assert.equal(cancelled.status, 200, `durable remote cancel accepted: ${JSON.stringify(cancelled.body)}`);
    // Another pool worker may win the cancellation-intent claim. The API confirms
    // durable acceptance; delivery/settlement then completes asynchronously.
    await waitForRemoteCancel(peer.cancels, 'cancel-task');
    assert.deepEqual(
      peer.cancels,
      ['cancel-task'],
      `cancel addressed the pinned remote task exactly once; sent=${JSON.stringify(peer.sent)} reads=${JSON.stringify(peer.reads)}`,
    );
    await waitForDispatchGone(cancelThread, cancelSubmit.body.run_id);
    await publishRemote(peer.endpoint);

    // A crash may expose old/corrupt continuation data written by a previous
    // binary or operator. Recovery must never invent a remote identity, switch
    // endpoint, or silently start a fresh task. Inject the damage directly into
    // this throwaway durable store: no production-only diagnostic API exists.
    const corruptions = await Promise.all(
      ['missing endpoint', 'missing task', 'missing context', 'endpoint mismatch', 'missing reference'].map(async (marker) => {
        const thread = await createSession();
        await sendText(thread, 'need remote input');
        return { marker, thread };
      }),
    );
    const corruptionKilled = new Promise<void>((resolve) => server.once('exit', () => resolve()));
    const corruptionThreads = corruptions.map(({ thread }) => thread).sort();
    // Recovery admission decision table: every created Session committed to the
    // durable aggregate => all exact ids survive restart; any missing id => fail
    // before interpreting the separately committed A2A continuation state.
    assert.deepEqual(persistedSessions(storage, corruptionThreads), corruptionThreads);
    server.kill('SIGKILL');
    await corruptionKilled;
    for (const { marker, thread } of corruptions) {
      rewriteLatestTaskReference(storage, thread, (command) => {
        if (marker === 'missing endpoint') delete command.action.Set.endpoint;
        else if (marker === 'missing task') delete command.action.Set.task_id;
        else if (marker === 'missing context') delete command.action.Set.context_id;
        else if (marker === 'endpoint mismatch') command.action.Set.endpoint = 'http://127.0.0.1:1';
        else command.action = 'Remove';
      });
    }
    server = spawnServer('config', PORT, environment).server;
    await waitForPort(PORT, 180_000, server);
    assert.deepEqual(persistedSessions(storage, corruptionThreads), corruptionThreads);
    for (const { marker, thread } of corruptions) await expectResumeFailure(thread, marker);
    assert.ok(
      !peer.sent.some((message) => corruptions.some(({ marker }) => message.text === marker)),
      'invalid durable continuations never reached the remote peer',
    );
    await publishRemote(peer.endpoint);

    // 4) Cancellation while a root remote attempt is actively polling uses the
    // same task driver as cold/awaiting cancellation and aborts the pinned task.
    const activeCancelThread = await createSession();
    const activeTurn = sendText(activeCancelThread, 'active remote cancel');
    const activeDeadline = Date.now() + 20_000;
    while (!peer.reads.includes('active-cancel-task') && Date.now() <= activeDeadline) await sleep(25);
    assert.ok(peer.reads.includes('active-cancel-task'), 'active remote task reached the poll boundary');
    const interrupted = await api('POST', `/v1/sessions/${activeCancelThread}/events`, {
      events: [{ type: 'user.interrupt' }],
    });
    assert.equal(interrupted.status, 200, `active remote interrupt accepted: ${JSON.stringify(interrupted.body)}`);
    await activeTurn.catch(() => {});
    await waitForRemoteCancel(peer.cancels, 'active-cancel-task');

    // Cancellation observes an already-terminal task and remains idempotent at
    // the remote boundary (no unnecessary tasks/cancel request).
    const terminalCancelThread = await createSession();
    const terminalSubmit = await api('POST', `/v1/durable/threads/${terminalCancelThread}/submit_background`, {
      agent: AGENT,
      text: 'cancel terminal remote',
    });
    assert.equal(terminalSubmit.status, 200);
    await waitForAwaiting(terminalCancelThread, terminalSubmit.body.run_id);
    const terminalCancel = await api('POST', `/v1/durable/threads/${terminalCancelThread}/cancel`, {
      run_id: terminalSubmit.body.run_id,
    });
    assert.equal(terminalCancel.status, 200, JSON.stringify(terminalCancel.body));
    await waitForDispatchGone(terminalCancelThread, terminalSubmit.body.run_id);
    assert.ok(!peer.cancels.includes('cancel-terminal-task'));

    // 5) Every terminal A2A state and every reply carrier is projected without
    // being collapsed to a false success. These are separate Sessions so their
    // committed run causes remain independently observable.
    for (const [prompt, marker] of [
      ['completed artifact', 'REMOTE-ARTIFACT-DONE'],
      ['failed terminal', 'REMOTE-FAILED-DONE'],
      ['rejected terminal', 'REMOTE-REJECTED-DONE'],
      ['canceled terminal', 'REMOTE-CANCELED-DONE'],
    ] as const) {
      const thread = await createSession();
      await sendText(thread, prompt);
      await waitForMessage(thread, marker);
      assert.ok(
        taskReferenceCleared(storage, thread),
        `${prompt} cleared its durable task reference`,
      );
    }

    // 6) auth-required is a first-class await boundary (distinct from user input)
    // and resumes on the exact committed context/task identity.
    const authThread = await createSession();
    await sendText(authThread, 'need remote auth');
    const authResume = await api('POST', `/v1/sessions/${authThread}/events`, {
      events: [
        {
          type: 'user.custom_tool_result',
          custom_tool_use_id: 'auth-task',
          content: [{ type: 'text', text: 'delegated-auth-ready' }],
          is_error: false,
        },
      ],
    });
    assert.equal(authResume.status, 200, `remote auth resumed: ${JSON.stringify(authResume.body)}`);
    await waitForMessage(authThread, 'REMOTE-AUTH-DONE');
    const authMessage = peer.sent.find((message) => message.text === 'delegated-auth-ready');
    assert.equal(authMessage?.contextId, 'auth-context', 'auth resume retained remote context');

    // 7) A remote send rejection is committed as an error outcome and never
    // fabricated into a successful answer.
    const errorThread = await createSession();
    await sendText(errorThread, 'trigger remote send rejection');
    await waitForMessage(errorThread, 'remote agent error');

    // A direct-ingress process makes transport error disposition immediately
    // observable (the durable process above intentionally retries such failures).
    await stopServer(server);
    server = spawnServer('config', PORT, {}).server;
    await waitForPort(PORT, 180_000, server);
    await publishRemote(peer.endpoint);
    const pollFailureThread = await createSession();
    const pollFailure = await api('POST', `/v1/sessions/${pollFailureThread}/events`, {
      events: [{ type: 'user.message', content: [{ type: 'text', text: 'poll remote failure' }] }],
    });
    assert.equal(pollFailure.status, 500, JSON.stringify(pollFailure.body));
    const resumeFailureThread = await createSession();
    await sendText(resumeFailureThread, 'resume remote failure');
    const resumeFailure = await api('POST', `/v1/sessions/${resumeFailureThread}/events`, {
      events: [{
        type: 'user.custom_tool_result',
        custom_tool_use_id: 'resume-failure-task',
        content: [{ type: 'text', text: 'continue' }],
        is_error: false,
      }],
    });
    assert.equal(resumeFailure.status, 500, JSON.stringify(resumeFailure.body));

    console.log(
      'REMOTE ATTEMPT TS API E2E PASS: crash reattach, input/auth resume, terminal states, send failure, and pinned-task cancellation.',
    );
  } finally {
    await stopServer(server).catch(() => {});
    await peer.close();
    fs.rmSync(storage, { recursive: true, force: true });
  }
}

main().catch((error) => {
  console.error('REMOTE ATTEMPT TS API E2E FAIL:', error);
  process.exitCode = 1;
});
