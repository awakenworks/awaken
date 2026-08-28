// Real-process API coverage for a published Agent whose pinned backend is A2A.
// The fake peer implements only the remote HTTP boundary; every Awaken component
// under test is production code: config projection/publication, session binding,
// durable dispatch, A2aRunExecutor, commit/readback, resume and cancellation.

import assert from 'node:assert/strict';
import fs, { mkdtempSync } from 'node:fs';
import http, { type IncomingMessage, type ServerResponse } from 'node:http';
import { tmpdir } from 'node:os';
import path from 'node:path';
import Anthropic from '@anthropic-ai/sdk';
import type {
  BetaManagedAgentsSessionEvent,
  BetaManagedAgentsUserToolResultEventParams,
} from '@anthropic-ai/sdk/resources/beta/sessions/events';
// @ts-ignore -- shared JavaScript harness intentionally serves TS scenarios.
import {
  assertPendingReceiptHasNoRuntimeEffects,
  hasEndTurn,
  spawnServer,
  stopServer,
  waitForPort,
  waitForSessionEventReceipt,
  waitForValue,
} from './harness.mjs';
// @ts-ignore -- shared JavaScript HTTP fixture intentionally serves TS scenarios.
import { closeHttpServer } from './http_server.mjs';
// @ts-ignore -- shared JavaScript SQLite fixture intentionally serves TS scenarios.
import { sqliteDatabaseForThread, sqliteExec, sqliteRows, sqliteRun } from './sqlite.mjs';

type SeenMessage = { messageId?: string; contextId?: string; text?: string };

const PORT = Number(process.env.E2E_PORT ?? 39771);
const BASE = `http://127.0.0.1:${PORT}`;
const AGENT = 'remote-root';
const BETAS = ['managed-agents-2026-04-01'];

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
      } else if (text.includes('trigger remote send rejection')) {
        json(response, 503, { error: { message: 'initial transport unavailable' } });
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
  // Raw HTTP is reserved for Awaken-only config/durable control surfaces, which
  // the Anthropic SDK does not own. C1=config/durable route => preserve its
  // extension wire; C2=Managed Session route => fail before I/O. E1=no second
  // Session codec survives beside the SDK. K1=fault injection remains direct DB
  // mutation below. Decision A1=C1=>fetch; A2=C2=>reject.
  assert.ok(
    route.startsWith('/v1/config/') || route.startsWith('/v1/durable/'),
    `compatible Managed route must use the Anthropic SDK: ${method} ${route}`,
  );
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

async function createSession(client: Anthropic): Promise<string> {
  // SDK create rule: C1=current-process client + valid typed Params => one
  // Session id. E1=the SDK owns request shape, beta and response decoding.
  // K1=a client is rebuilt after every server restart. Decision S1=C1=>E1.
  const created = await within(client.beta.sessions.create({
    agent: AGENT,
    environment_id: 'env_local',
    betas: BETAS,
  }), 30_000, 'Managed Session create');
  assert.ok(created.id, `session created: ${JSON.stringify(created)}`);
  return created.id;
}

type ToolUseEvent = Extract<
  BetaManagedAgentsSessionEvent,
  { type: 'agent.tool_use' }
>;

async function sessionEvents(
  client: Anthropic,
  thread: string,
): Promise<BetaManagedAgentsSessionEvent[]> {
  return within((async () => {
    const events: BetaManagedAgentsSessionEvent[] = [];
    for await (const event of client.beta.sessions.events.list(thread, { betas: BETAS })) {
      events.push(event);
    }
    return events;
  })(), 5_000, `official Session Event history for ${thread}`);
}

async function waitForPendingTool(
  client: Anthropic,
  thread: string,
  receiptId: string,
): Promise<ToolUseEvent> {
  // Reply-target cause/effect table: C1=exact User receipt is processed;
  // C2=its delta's latest idle is requires_action; C3=that edge names one
  // qualified agent.tool_use. E1=return only that public id. K1=older idle/tool
  // Events, raw A2A task ids and unlisted tools are ineligible. Decision R1
  // !C1||!C2||!C3=>retry; R2=C1+C2+C3=>E1.
  const { delta }: { delta: BetaManagedAgentsSessionEvent[] } = await waitForSessionEventReceipt(
    client,
    thread,
    receiptId,
    BETAS,
    ({ delta: observed }: { delta: BetaManagedAgentsSessionEvent[] }) => {
      const latestIdle = [...observed]
        .reverse()
        .find((event) => event.type === 'session.status_idle');
      return latestIdle?.stop_reason.type === 'requires_action'
        && latestIdle.stop_reason.event_ids.some((id) => observed.some(
          (event) => event.type === 'agent.tool_use' && event.id === id,
        ));
    },
    `Session ${thread} exact receipt to publish its qualified custom-tool reply target`,
    { timeoutMs: 30_000 },
  );
  const latestIdle = [...delta]
    .reverse()
    .find((event) => event.type === 'session.status_idle');
  assert.equal(latestIdle?.stop_reason.type, 'requires_action');
  const pendingIds = latestIdle.stop_reason.event_ids;
  const pending = delta.filter((event): event is ToolUseEvent =>
    event.type === 'agent.tool_use' && pendingIds.includes(event.id));
  assert.equal(pending.length, 1, `one qualified pending generic tool: ${JSON.stringify(delta)}`);
  assert.equal(pending[0].name, 'agent_input', 'A2A input/auth await projects agent_input');
  assert.equal(pending[0].evaluated_permission, 'allow', 'A2A agent_input is client-answerable');
  return pending[0];
}

function toolResult(
  toolUseId: string,
  text: string,
): BetaManagedAgentsUserToolResultEventParams {
  return {
    type: 'user.tool_result',
    tool_use_id: toolUseId,
    content: [{ type: 'text', text }],
    is_error: false,
  };
}

async function sendText(
  client: Anthropic,
  thread: string,
  text: string,
): Promise<BetaManagedAgentsSessionEvent> {
  // Admission rule: C1=one typed User message => SDK returns one exact durable
  // receipt. E1=callers bind their own later effect to that id. K1=HTTP success
  // alone never proves processing or terminal state. Decision M1=C1=>E1.
  const response = await within(client.beta.sessions.events.send(thread, {
    events: [{ type: 'user.message', content: [{ type: 'text', text }] }],
    betas: BETAS,
  }), 30_000, `Managed Event admission for ${text}`);
  assert.equal(response.data?.length, 1, `${text}: ${JSON.stringify(response)}`);
  const receipt = response.data?.[0];
  assert.equal(typeof receipt?.id, 'string', `${text} returns one exact User Event receipt`);
  return receipt;
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

async function expectResumeFailure(
  client: Anthropic,
  thread: string,
  marker: string,
  toolUseId: string,
): Promise<void> {
  const expected = new Map([
    ['missing endpoint', 'missing field `endpoint`'],
    ['missing task', 'missing field `task_id`'],
    ['missing context', 'missing field `context_id`'],
    ['endpoint mismatch', 'belongs to endpoint'],
    ['missing reference', 'missing its durable remote task'],
  ]).get(marker)!;
  const response = await within(client.beta.sessions.events.send(thread, {
    events: [toolResult(toolUseId, marker)],
    betas: BETAS,
  }), 5_000, `${marker} corrupt continuation admission`);
  const receipt = response.data?.[0];
  assert.ok(receipt && typeof receipt.id === 'string', `${marker} returns an exact tool-result receipt`);
  // Corrupt-reference cause/effect decision table. C1 a required typed field is
  // absent, C2 the endpoint differs, or C3 the reference was removed; C4 the
  // exact SDK receipt is processed. E1=the corresponding typed Session error and
  // idle edge follow only that receipt; E2=the dispatch settles; E3=no peer send.
  // K1=these are fail-closed diagnostics, not fallback codecs/recovery paths.
  // Decision D1=(C1||C2||C3)+C4=>E1+E2+E3.
  await waitForSessionEventReceipt(
    client,
    thread,
    receipt.id,
    BETAS,
    ({ delta }: { delta: BetaManagedAgentsSessionEvent[] }) => delta.some(
      (event) => event.type === 'session.error'
        && event.error?.message.includes(expected),
    ) && delta.some((event) => event.type === 'session.status_idle'),
    `${marker} exact receipt to commit its error and idle boundary`,
    { timeoutMs: 5_000, pollMs: 25 },
  );
  await waitForMessage(thread, expected, 5_000);
  await waitForValue(
    () => api('GET', `/v1/durable/threads/${thread}/dispatches`),
    (dispatches: { status: number; body: any }) => dispatches.status === 200
      && (dispatches.body.dispatches ?? []).length === 0,
    `${marker} terminal failure dispatch to settle`,
    { timeoutMs: 5_000, pollMs: 25 },
  );
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
  return waitForValue(
    async () => {
      const response = await api('GET', `/v1/durable/threads/${thread}/messages`);
      assert.equal(response.status, 200, `list Thread messages: ${JSON.stringify(response.body)}`);
      return response.body.messages ?? [];
    },
    (messages: any[]) => JSON.stringify(messages).includes(marker),
    `Thread ${thread} message ${marker}`,
    { timeoutMs, pollMs: 100 },
  );
}

async function waitForAwaiting(thread: string, runId: string): Promise<void> {
  await waitForValue(
    () => api('GET', `/v1/durable/threads/${thread}/dispatches`),
    (response: { status: number; body: any }) => response.status === 200
      && (response.body.dispatches ?? []).some(
        (entry: any) => entry.run_id === runId && entry.status === 'Awaiting',
      ),
    `Run ${runId} to reach Awaiting`,
    { timeoutMs: 20_000, pollMs: 50 },
  );
}

async function waitForRemoteCancel(cancels: string[], taskId: string): Promise<void> {
  await waitForValue(
    () => [...cancels],
    (observed: string[]) => observed.includes(taskId),
    `remote cancellation to reach ${taskId}`,
    { timeoutMs: 20_000, pollMs: 25 },
  );
}

async function waitForDispatchGone(thread: string, runId: string): Promise<void> {
  await waitForValue(
    () => api('GET', `/v1/durable/threads/${thread}/dispatches`),
    (response: { status: number; body: any }) => response.status === 200
      && !(response.body.dispatches ?? []).some((entry: any) => entry.run_id === runId),
    `cancelled Run ${runId} to leave dispatchable state`,
    { timeoutMs: 20_000, pollMs: 25 },
  );
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
  let client = new Anthropic({
    apiKey: 'e2e-dummy',
    baseURL: BASE,
    maxRetries: 0,
    timeout: 30_000,
  });
  try {
    await waitForPort(PORT, 180_000, server);
    await publishRemote(peer.endpoint);

    // 1) Crash after task-reference commit but during tasks/get. Replacement must
    // reattach to crash-task from the pinned snapshot and never message:send again.
    const crashThread = await createSession(client);
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
    client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: BASE, maxRetries: 0, timeout: 30_000 });
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

    // 2) A foreground Managed API Run reaches input-required. A client result
    // resumes the root Run on the exact remote context and commits its terminal reply.
    const inputThread = await createSession(client);
    const firstReceipt = await sendText(client, inputThread, 'need remote input');
    const inputToolUse = await waitForPendingTool(client, inputThread, firstReceipt.id);
    const resumed = await within(client.beta.sessions.events.send(inputThread, {
      events: [toolResult(inputToolUse.id, 'README.md')],
      betas: BETAS,
    }), 30_000, 'remote input resume');
    const resumedReceipt = resumed.data?.[0];
    assert.ok(resumedReceipt && typeof resumedReceipt.id === 'string', 'remote input resume returns one exact receipt');
    // Resume rule: C1=qualified public agent_input id; C2=exact SDK result
    // receipt; C3=its delta carries REMOTE-RESUME-DONE and latest idle/end_turn.
    // E1=the pinned A2A context resumes once. K1=pre-resume history cannot
    // satisfy C3. Decision R3=C1+C2+C3=>E1.
    await waitForSessionEventReceipt(
      client,
      inputThread,
      resumedReceipt.id,
      BETAS,
      ({ delta }: { delta: BetaManagedAgentsSessionEvent[] }) =>
        JSON.stringify(delta).includes('REMOTE-RESUME-DONE') && hasEndTurn(delta),
      'remote input exact receipt to commit its marker and idle/end_turn',
      { timeoutMs: 30_000 },
    );
    await waitForMessage(inputThread, 'REMOTE-RESUME-DONE');
    const resumeMessage = peer.sent.find((message) => message.text === 'README.md');
    assert.equal(resumeMessage?.contextId, 'input-context', 'resume retained remote context');
    assert.match(resumeMessage?.messageId ?? '', /^a2a-resume-/, 'resume used stable run/ticket identity');

    // 3) An awaiting background root Run is cancelled through the durable API.
    // The cancellation resolver reconstructs the remote executor without model,
    // credential or sandbox dependencies and addresses the committed task id.
    const cancelThread = await createSession(client);
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
    client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: BASE, maxRetries: 0, timeout: 30_000 });
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
        const thread = await createSession(client);
        const receipt = await sendText(client, thread, 'need remote input');
        const toolUse = await waitForPendingTool(client, thread, receipt.id);
        return { marker, thread, toolUseId: toolUse.id };
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
    client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: BASE, maxRetries: 0, timeout: 30_000 });
    await waitForPort(PORT, 180_000, server);
    assert.deepEqual(persistedSessions(storage, corruptionThreads), corruptionThreads);
    // Restore the exact publication first so each request reaches the damaged
    // A2A continuation boundary. Otherwise the publication's correct 503 masks
    // every corruption case and the test proves only fail-closed startup.
    await publishRemote(peer.endpoint);
    for (const { marker, thread, toolUseId } of corruptions) {
      await expectResumeFailure(client, thread, marker, toolUseId);
    }
    assert.ok(
      !peer.sent.some((message) => corruptions.some(({ marker }) => message.text === marker)),
      'invalid durable continuations never reached the remote peer',
    );
    // 4) Cancellation while a root remote attempt is actively polling uses the
    // same task driver as cold/awaiting cancellation and aborts the pinned task.
    const activeCancelThread = await createSession(client);
    const activeRunAdmission = sendText(client, activeCancelThread, 'active remote cancel');
    await waitForValue(
      () => [...peer.reads],
      (reads: string[]) => reads.includes('active-cancel-task'),
      'active remote task to reach the poll boundary',
      { timeoutMs: 20_000, pollMs: 25 },
    );
    const interrupted = await within(client.beta.sessions.events.send(activeCancelThread, {
      events: [{ type: 'user.interrupt' }],
      betas: BETAS,
    }), 30_000, 'active remote interrupt admission');
    const interruptReceipt = interrupted.data?.[0];
    assert.ok(interruptReceipt && typeof interruptReceipt.id === 'string', 'active interrupt returns one exact receipt');
    await activeRunAdmission;
    await waitForRemoteCancel(peer.cancels, 'active-cancel-task');
    // Interrupt rule: C1=exact SDK interrupt receipt; C2=the peer receives cancel
    // for the pinned task. E1=C1 is processed and C2 occurs once. K1=peer state is
    // the side-effect oracle; history only owns the receipt fence. D1=C1+C2=>E1.
    await waitForSessionEventReceipt(
      client,
      activeCancelThread,
      interruptReceipt.id,
      BETAS,
      () => true,
      'active interrupt receipt to process after pinned peer cancellation',
      { timeoutMs: 30_000 },
    );

    // Cancellation observes an already-terminal task and remains idempotent at
    // the remote boundary (no unnecessary tasks/cancel request).
    const terminalCancelThread = await createSession(client);
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
      const thread = await createSession(client);
      const receipt = await sendText(client, thread, prompt);
      // Terminal-carrier rule: C1=exact prompt receipt; C2=the A2A terminal is
      // committed. E1=C1 is processed before the durable marker is accepted as
      // evidence. K1=failed/rejected/canceled carriers keep their own terminal
      // semantics, so this fence does not demand end_turn. D1=C1+C2=>E1.
      await waitForSessionEventReceipt(
        client,
        thread,
        receipt.id,
        BETAS,
        () => true,
        `${prompt} exact receipt to process`,
        { timeoutMs: 30_000 },
      );
      await waitForMessage(thread, marker);
      assert.ok(
        taskReferenceCleared(storage, thread),
        `${prompt} cleared its durable task reference`,
      );
    }

    // 6) auth-required is a first-class await boundary (distinct from user input)
    // and resumes on the exact committed context/task identity.
    const authThread = await createSession(client);
    const authReceipt = await sendText(client, authThread, 'need remote auth');
    const authToolUse = await waitForPendingTool(client, authThread, authReceipt.id);
    const authResume = await within(client.beta.sessions.events.send(authThread, {
      events: [toolResult(authToolUse.id, 'delegated-auth-ready')],
      betas: BETAS,
    }), 30_000, 'remote auth resume');
    const authResumeReceipt = authResume.data?.[0];
    assert.ok(authResumeReceipt && typeof authResumeReceipt.id === 'string', 'remote auth resume returns one exact receipt');
    // Auth-resume rule: C1=qualified auth agent_input; C2=exact result receipt;
    // C3=its delta carries REMOTE-AUTH-DONE and idle/end_turn. E1=the exact
    // committed context resumes once. K1=older success is excluded. D1=C1+C2+C3=>E1.
    await waitForSessionEventReceipt(
      client,
      authThread,
      authResumeReceipt.id,
      BETAS,
      ({ delta }: { delta: BetaManagedAgentsSessionEvent[] }) =>
        JSON.stringify(delta).includes('REMOTE-AUTH-DONE') && hasEndTurn(delta),
      'remote auth exact receipt to commit its marker and idle/end_turn',
      { timeoutMs: 30_000 },
    );
    await waitForMessage(authThread, 'REMOTE-AUTH-DONE');
    const authMessage = peer.sent.find((message) => message.text === 'delegated-auth-ready');
    assert.equal(authMessage?.contextId, 'auth-context', 'auth resume retained remote context');

    // 7) Remote transport-failure cause/effect graph. C1 the initial
    // message:send returns 503 before a task identity exists; C2 a committed
    // working task's exact tasks/get returns 503; C3 an awaiting task resumes on
    // its committed context but that message:send response is lost with 503.
    // Effects: E1 every admitted User Event receives the exact HTTP 200 receipt,
    // whose processed_at is nullable while asynchronous admission is in flight;
    // E2 C1 commits the terminal a2a_error; E3 C2/C3 reach the exact remote
    // task/context while the unanchored receipt remains excluded from committed
    // history and the Session/Run remains Running;
    // E4 no rule fabricates an Agent success or terminal Session boundary.
    // Constraint: ADR-0057 makes poll/cancel delivery failure and resume response
    // loss retryable after remote identity exists; only the rejected initial send
    // is terminal because there is no task to reattach.
    //
    // | Rule | Initial send | Task committed | Poll/resume 503 | Effects |
    // | F1 | 503 | no | n/a | E1 + E2 + E4 |
    // | F2 | 200 | yes | poll | E1 + E3 + E4 |
    // | F3 | 200 | yes, awaiting | resume | E1 + E3 + E4 |
    const errorThread = await createSession(client);
    const errorReceipt = await sendText(client, errorThread, 'trigger remote send rejection');
    await waitForSessionEventReceipt(
      client,
      errorThread,
      errorReceipt.id,
      BETAS,
      ({ delta }: { delta: BetaManagedAgentsSessionEvent[] }) => delta.some(
        (event) => event.type === 'session.error'
          && event.error?.message.includes('503'),
      ),
      'F1 exact receipt to commit its terminal A2A send error',
      { timeoutMs: 30_000 },
    );
    const initialFailureMessages = await waitForMessage(
      errorThread,
      'remote agent error',
    );
    assert.ok(
      JSON.stringify(initialFailureMessages).includes('503'),
      `F1/E2 committed the explicit initial-send 503: ${JSON.stringify(initialFailureMessages)}`,
    );

    // A process without the durable monitoring surface still uses the same
    // Session Event batch admission and remote-attempt authority.
    await stopServer(server);
    server = spawnServer('config', PORT, {}).server;
    client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: BASE, maxRetries: 0, timeout: 30_000 });
    await waitForPort(PORT, 180_000, server);
    await publishRemote(peer.endpoint);
    const pollFailureThread = await createSession(client);
    const pollPriorHistory = await sessionEvents(client, pollFailureThread);
    const pollReadStart = peer.reads.length;
    const pollFailure = await within(
      client.beta.sessions.events.send(pollFailureThread, {
        events: [
          {
            type: 'user.message',
            content: [{ type: 'text', text: 'poll remote failure' }],
          },
        ],
        betas: BETAS,
      }),
      30_000,
      'F2 retryable poll admission',
    );
    assert.equal(
      pollFailure.data?.length,
      1,
      `F2/E1 one receipt: ${JSON.stringify(pollFailure)}`,
    );
    const pollFailureReceipt = pollFailure.data?.[0];
    assert.ok(pollFailureReceipt, 'F2/E1 exact receipt exists');
    assert.equal(
      pollFailureReceipt.type,
      'user.message',
      'F2/E1 preserves the admitted Event type',
    );
    assert.equal(
      typeof pollFailureReceipt.id,
      'string',
      'F2/E1 assigns the durable Event identity',
    );
    assert.equal(pollFailureReceipt.processed_at, null, 'F2/E1 retryable receipt is unprocessed');
    // F2 negative-observation rule: C1=the official SDK receipt is admitted;
    // C2=the pinned task is polled and returns retryable 503; C3=Session remains
    // Running. E1=the receipt is exposed exactly once as unprocessed history and
    // no success/error/idle/terminated delta is fabricated. D1=C1+C2+C3=>E1.
    const retryablePoll: {
      session: { status: string };
      events: BetaManagedAgentsSessionEvent[];
      reads: string[];
    } = await waitForValue(
      async () => ({
        session: await within(
          client.beta.sessions.retrieve(pollFailureThread, { betas: BETAS }),
          5_000,
          'F2 official Session retrieve',
        ),
        events: await sessionEvents(client, pollFailureThread),
        reads: peer.reads.slice(pollReadStart),
      }),
      (observed: {
        session: { status: string };
        events: BetaManagedAgentsSessionEvent[];
        reads: string[];
      }) =>
        observed.session.status === 'running' &&
        observed.reads.includes('poll-failure-task'),
      'F2 retryable exact remote poll',
      { timeoutMs: 20_000, pollMs: 25 },
    );
    assert.ok(
      retryablePoll.reads.length > 0 &&
        retryablePoll.reads.every((taskId) => taskId === 'poll-failure-task'),
      `F2/E3 every new poll addresses the committed task: ${JSON.stringify(retryablePoll.reads)}`,
    );
    assertPendingReceiptHasNoRuntimeEffects({
      history: retryablePoll.events,
      priorHistory: pollPriorHistory,
      receiptId: pollFailureReceipt.id,
      forbiddenEventTypes: new Set([
        'agent.message',
        'session.error',
        'session.status_idle',
        'session.status_terminated',
      ]),
      description: 'F2 retryable remote poll',
    });
    const resumeFailureThread = await createSession(client);
    const resumeStartReceipt = await sendText(client, resumeFailureThread, 'resume remote failure');
    const resumeFailureToolUse = await waitForPendingTool(
      client,
      resumeFailureThread,
      resumeStartReceipt.id,
    );
    const resumePriorHistory = await sessionEvents(client, resumeFailureThread);
    const resumeSendStart = peer.sent.length;
    const resumeFailure = await within(
      client.beta.sessions.events.send(resumeFailureThread, {
        events: [toolResult(resumeFailureToolUse.id, 'continue')],
        betas: BETAS,
      }),
      30_000,
      'F3 retryable resume admission',
    );
    assert.equal(
      resumeFailure.data?.length,
      1,
      `F3/E1 one receipt: ${JSON.stringify(resumeFailure)}`,
    );
    const resumeFailureReceipt = resumeFailure.data?.[0];
    assert.ok(resumeFailureReceipt, 'F3/E1 exact receipt exists');
    assert.equal(
      resumeFailureReceipt.type,
      'user.tool_result',
      'F3/E1 preserves the admitted Event type',
    );
    assert.equal(
      typeof resumeFailureReceipt.id,
      'string',
      'F3/E1 assigns the durable Event identity',
    );
    assert.equal(
      typeof resumeFailureReceipt.processed_at,
      'string',
      'F3/E1 the result receipt is processed against the committed ToolUse anchor',
    );
    // F3 negative-observation rule: C1=the official SDK result receipt is
    // processed against the committed ToolUse anchor; C2=retry reaches the
    // exact remote context with stable identity; C3=Session remains Running;
    // C4=the remote response is lost with retryable 503; C5=the prior awaiting
    // boundary can sort after the result receipt by its own causal anchor.
    // E1=the exact receipt is committed; E2=no new success/error/idle/terminated
    // event is fabricated. Decision D1=C1+C2+C3+C4+C5=>E1+E2.
    const retryableResume: {
      session: { status: string };
      events: BetaManagedAgentsSessionEvent[];
      sent: SeenMessage[];
    } = await waitForValue(
      async () => ({
        session: await within(
          client.beta.sessions.retrieve(resumeFailureThread, { betas: BETAS }),
          5_000,
          'F3 official Session retrieve',
        ),
        events: await sessionEvents(client, resumeFailureThread),
        sent: peer.sent.slice(resumeSendStart),
      }),
      (observed: {
        session: { status: string };
        events: BetaManagedAgentsSessionEvent[];
        sent: SeenMessage[];
      }) =>
        observed.session.status === 'running' &&
        observed.sent.some(
          (message) =>
            message.text === 'continue' &&
            message.contextId === 'resume-failure-context',
        ),
      'F3 retryable exact remote resume',
      { timeoutMs: 20_000, pollMs: 25 },
    );
    const retryableResumeSends = retryableResume.sent.filter(
      (message) => message.text === 'continue',
    );
    assert.ok(
      retryableResumeSends.length > 0,
      'F3/E3 reached the remote resume boundary',
    );
    assert.ok(
      retryableResumeSends.every(
        (message) =>
          message.contextId === 'resume-failure-context' &&
          /^a2a-resume-/.test(message.messageId ?? ''),
      ),
      `F3/E3 every retry retains the committed context and stable identity shape: ${JSON.stringify(retryableResumeSends)}`,
    );
    const committedResumeReceiptIndex = retryableResume.events.findIndex(
      (event) => event.id === resumeFailureReceipt.id && event.processed_at,
    );
    assert.ok(
      committedResumeReceiptIndex >= 0,
      'F3/E1 exact processed result receipt is retained in committed history',
    );
    const resumePriorEventIds = new Set(resumePriorHistory.map((event) => event.id));
    const forbiddenResumeEventTypes = new Set([
      'agent.message',
      'session.error',
      'session.status_idle',
      'session.status_terminated',
    ]);
    const forbiddenResumeEffects = retryableResume.events
      .filter((event) => !resumePriorEventIds.has(event.id))
      .filter((event) => forbiddenResumeEventTypes.has(event.type));
    assert.deepEqual(
      forbiddenResumeEffects,
      [],
      'F3/E2 retryable resume does not fabricate execution or terminal effects',
    );
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
