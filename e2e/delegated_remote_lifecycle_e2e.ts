// Real-process lifecycle coverage for a remote child Run created by `agent_run`.
//
// A deterministic HTTP peer is the only test double. The coordinator, Managed
// API, durable continuation, cancellation token, A2A adapter and parent/child Run
// projection are production code.

import assert from 'node:assert/strict';
import http, { type IncomingMessage, type ServerResponse } from 'node:http';
import Anthropic from '@anthropic-ai/sdk';
import {
  agentCardSecurityFingerprint,
  pass,
  realServerEnv,
  spawnServer,
  startUpstream,
  stopServer,
  waitForPort,
} from './harness.mjs';
import { closeHttpServer } from './http_server.mjs';

const PORT = Number(process.env.E2E_PORT ?? 38214);
const BETAS = ['managed-agents-2026-04-01'];
const sleep = (ms: number): Promise<void> => new Promise((resolve) => setTimeout(resolve, ms));

// SDK boundary decision table: operation settles before 30s -> preserve its
// result/error; operation remains unresolved after transport/process loss ->
// fail the owning lifecycle rule with its phase label. No test rule may retain
// fixture listeners forever behind a handle-free Promise.
async function within<T>(promise: Promise<T>, label: string, timeoutMs = 30_000): Promise<T> {
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

type PeerState = {
  freshMessages: number;
  polls: string[];
  cancels: string[];
  resumes: Array<{ contextId?: string; text: string }>;
};

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

async function body(request: IncomingMessage): Promise<any> {
  const chunks: Buffer[] = [];
  for await (const chunk of request) chunks.push(Buffer.from(chunk));
  const encoded = Buffer.concat(chunks).toString('utf8');
  return encoded ? JSON.parse(encoded) : {};
}

function json(response: ServerResponse, status: number, value?: unknown): void {
  response.writeHead(status, { 'content-type': 'application/json' });
  response.end(value === undefined ? undefined : JSON.stringify(value));
}

async function startPeer(): Promise<{ endpoint: string; state: PeerState; close: () => Promise<void> }> {
  const state: PeerState = { freshMessages: 0, polls: [], cancels: [], resumes: [] };
  const sequenceByContext = new Map<string, number>();
  const server = http.createServer(async (request, response) => {
    const route = request.url ?? '';
    if (request.method === 'POST' && route === '/v1/a2a/message:send') {
      const requestBody = await body(request);
      const message = requestBody.message ?? {};
      const text = (message.parts ?? []).map((part: any) => String(part.text ?? '')).join('');
      if (message.contextId === 'delegated-input-context') {
        state.resumes.push({ contextId: message.contextId, text });
        json(response, 200, {
          task: task('delegated-resumed', 'delegated-input-context', 'completed', 'REMOTE-CHILD-RESUMED'),
        });
        return;
      }

      if (text.includes('delegate lifecycle: poll failure')) {
        json(response, 200, {
          task: task('delegated-poll-failure', 'delegated-poll-failure-context', 'working'),
        });
        return;
      }
      if (text.includes('delegate lifecycle: failed')) {
        json(response, 200, { task: task('delegated-failed', 'delegated-failed-context', 'failed') });
        return;
      }
      if (text.includes('delegate lifecycle: rejected')) {
        json(response, 200, { task: task('delegated-rejected', 'delegated-rejected-context', 'rejected') });
        return;
      }
      if (text.includes('delegate lifecycle: canceled')) {
        json(response, 200, { task: task('delegated-canceled', 'delegated-canceled-context', 'canceled') });
        return;
      }
      if (text.includes('delegate lifecycle: unavailable')) {
        json(response, 503, { error: { message: 'delegated peer unavailable' } });
        return;
      }

      const identity = String(message.contextId ?? message.messageId ?? 'missing-context');
      let sequence = sequenceByContext.get(identity);
      if (sequence === undefined) {
        state.freshMessages += 1;
        sequence = state.freshMessages;
        sequenceByContext.set(identity, sequence);
      }
      switch (sequence) {
        case 1:
          json(response, 200, {
            task: task('delegated-input', 'delegated-input-context', 'input-required', 'which target?'),
          });
          return;
        case 2:
          json(response, 200, {
            task: task('delegated-polled', 'delegated-poll-context', 'working'),
          });
          return;
        case 3:
          json(response, 200, {
            task: task('delegated-cancel', 'delegated-cancel-context', 'working'),
          });
          return;
        default:
          json(response, 503, { error: { message: 'delegated peer unavailable' } });
          return;
      }
    }

    const get = route.match(/^\/v1\/a2a\/tasks\/([^/]+)$/);
    if (request.method === 'GET' && get) {
      state.polls.push(get[1]);
      if (get[1] === 'delegated-polled') {
        json(response, 200, task('delegated-polled', 'delegated-poll-context', 'completed', 'REMOTE-CHILD-POLLED'));
      } else if (get[1] === 'delegated-cancel') {
        json(response, 200, task('delegated-cancel', 'delegated-cancel-context', 'working'));
      } else if (get[1] === 'delegated-poll-failure') {
        json(response, 503, { error: { message: 'delegated poll unavailable' } });
      } else {
        json(response, 404, { error: { message: `unknown task ${get[1]}` } });
      }
      return;
    }

    const cancel = route.match(/^\/v1\/a2a\/tasks\/([^/]+):cancel$/);
    if (request.method === 'POST' && cancel) {
      state.cancels.push(cancel[1]);
      json(response, 204);
      return;
    }

    if (request.method === 'GET' && route === '/v1/a2a/agent-card') {
      json(response, 200, {
        name: 'remote-researcher',
        description: 'delegated lifecycle peer',
        url: `http://${request.headers.host}`,
        version: '1',
        protocolVersion: '0.3.0',
        preferredTransport: 'HTTP+JSON',
        capabilities: {},
        defaultInputModes: ['text/plain'],
        defaultOutputModes: ['text/plain'],
        skills: [],
      });
      return;
    }

    json(response, 404, { error: { message: `unexpected route ${request.method} ${route}` } });
  });
  await new Promise<void>((resolve) => server.listen(0, '127.0.0.1', resolve));
  const address = server.address();
  assert.ok(address && typeof address !== 'string');
  return {
    endpoint: `http://127.0.0.1:${address.port}`,
    state,
    close: () => closeHttpServer(server),
  };
}

async function listEvents(client: Anthropic, sessionId: string): Promise<any[]> {
  return within((async () => {
    const events: any[] = [];
    for await (const event of client.beta.sessions.events.list(sessionId, { betas: BETAS })) events.push(event);
    return events;
  })(), `events for ${sessionId}`);
}

async function createSession(client: Anthropic): Promise<any> {
  return within(
    client.beta.sessions.create({ agent: 'assistant', environment_id: 'env_local', betas: BETAS }),
    'Session create',
  );
}

async function sendText(client: Anthropic, sessionId: string, text: string): Promise<void> {
  await within(
    client.beta.sessions.events.send(sessionId, {
      events: [{ type: 'user.message', content: [{ type: 'text', text }] }],
      betas: BETAS,
    }),
    `turn ${text}`,
  );
}

async function waitFor<T>(read: () => T | undefined, label: string): Promise<T> {
  const deadline = Date.now() + 20_000;
  while (Date.now() <= deadline) {
    const value = read();
    if (value !== undefined) return value;
    await sleep(25);
  }
  throw new Error(`timed out waiting for ${label}`);
}

async function main(): Promise<void> {
  const peer = await startPeer();
  const upstream = await startUpstream('delegating');
  let server;
  try {
    const cardResponse = await within(
      fetch(`${peer.endpoint}/v1/a2a/agent-card`),
      'remote Agent Card',
    );
    assert.equal(cardResponse.status, 200, 'remote Agent Card is discoverable before pinning');
    const securityFingerprint = agentCardSecurityFingerprint(await cardResponse.json());
    server = spawnServer('delegate-remote', PORT, {
      AWAKEN_REMOTE_AGENT_URL: peer.endpoint,
      AWAKEN_REMOTE_AGENT_SECURITY_FINGERPRINT: securityFingerprint,
      ...realServerEnv('delegating', upstream, { mode: 'delegate-remote' }),
    });
    const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: `http://127.0.0.1:${PORT}` });
    // Ownership decision table: successful readiness -> drive lifecycle rules;
    // startup exit/error/timeout -> enter this same finally and close every
    // already-listening fixture. Readiness must never sit outside its resources'
    // cleanup scope.
    await waitForPort(PORT, 180_000, server.server);
    // A remote child may pause for user input. Cause/effect decision table:
    // built-in agent_run + remote input-required -> the executed agent.tool_use
    // remains in history, while an answerable agent.custom_tool_use(agent_input)
    // is minted at the remote pending id; its user.custom_tool_result resumes the
    // exact task context. The executed parent call must never be mistaken for the
    // later remote-input ticket.
    const awaiting = await createSession(client);
    await sendText(client, awaiting.id, 'delegate and wait for remote input');
    const awaitingEvents = await listEvents(client, awaiting.id);
    const idle = awaitingEvents.find(
      (event) => event.type === 'session.status_idle' && event.stop_reason?.type === 'requires_action',
    );
    const pendingId = idle?.stop_reason?.event_ids?.[0];
    const toolUse = awaitingEvents.find(
      (event) => event.id === pendingId && event.type === 'agent.custom_tool_use' && event.name === 'agent_input',
    );
    assert.ok(toolUse, `remote agent_input was projected: ${JSON.stringify(awaitingEvents)}`);
    await within(client.beta.sessions.events.send(awaiting.id, {
      events: [
        {
          type: 'user.custom_tool_result',
          custom_tool_use_id: toolUse.id,
          content: [{ type: 'text', text: 'src/lib.rs' }],
          is_error: false,
        },
      ],
      betas: BETAS,
    }), 'remote child input resume');
    const resumedEvents = await listEvents(client, awaiting.id);
    assert.ok(JSON.stringify(resumedEvents).includes('REMOTE-CHILD-RESUMED'));
    assert.deepEqual(peer.state.resumes, [{ contextId: 'delegated-input-context', text: 'src/lib.rs' }]);
    pass('remote agent_run input-required resumes on the pinned task context');

    // Active remote children share the common A2A poll driver.
    const polled = await createSession(client);
    await sendText(client, polled.id, 'delegate and poll remote child');
    const polledEvents = await listEvents(client, polled.id);
    assert.ok(JSON.stringify(polledEvents).includes('REMOTE-CHILD-POLLED'));
    assert.ok(peer.state.polls.includes('delegated-polled'));
    pass('remote agent_run polls a working task to its terminal result');

    // Interrupt the parent while its child is polling. Cancellation must cross
    // the same token into A2A and address the pinned child task once.
    const cancelled = await createSession(client);
    const activeTurn = sendText(client, cancelled.id, 'delegate then interrupt remote child');
    await waitFor(() => (peer.state.polls.includes('delegated-cancel') ? true : undefined), 'remote child poll');
    await within(client.beta.sessions.events.send(cancelled.id, {
      events: [{ type: 'user.interrupt' }],
      betas: BETAS,
    }), 'parent interrupt');
    await within(activeTurn.catch(() => {}), 'interrupted parent turn');
    await waitFor(() => peer.state.cancels.find((id) => id === 'delegated-cancel'), 'remote child cancellation');
    assert.deepEqual(peer.state.cancels, ['delegated-cancel']);
    pass('parent interrupt cancels the pinned remote child task exactly once');

    // Cause/effect rule: poll/send transport 5xx and every negative terminal
    // become an error ToolResult for the parent to observe and explain; the
    // Managed send request itself may therefore complete normally. None may be
    // collapsed into a fabricated successful child result.
    const pollFailed = await createSession(client);
    await sendText(client, pollFailed.id, 'delegate lifecycle: poll failure');
    const pollFailureProjection = JSON.stringify(await listEvents(client, pollFailed.id));
    assert.ok(pollFailureProjection.includes('503'), pollFailureProjection);
    assert.ok(!pollFailureProjection.includes('REMOTE-CHILD-'));
    assert.ok(peer.state.polls.includes('delegated-poll-failure'));
    pass('remote agent_run poll 5xx remains an explicit child tool error');

    // Protocol terminal failures are deterministic, non-retryable tool errors.
    // The parent may observe and explain that error, but never receives a
    // fabricated successful child payload.
    for (const [prompt, marker] of [
      ['delegate lifecycle: failed', 'a2a_task_failed'],
      ['delegate lifecycle: rejected', 'a2a_task_rejected'],
      ['delegate lifecycle: canceled', 'Cancelled'],
    ]) {
      const failed = await createSession(client);
      await sendText(client, failed.id, prompt);
      const failedProjection = JSON.stringify(await listEvents(client, failed.id));
      assert.ok(failedProjection.includes(marker), `${prompt}: ${failedProjection}`);
      assert.ok(!failedProjection.includes('REMOTE-CHILD-'), prompt);
    }
    pass('remote failed/rejected/canceled terminals remain explicit child tool errors');

    const sendFailed = await createSession(client);
    await sendText(client, sendFailed.id, 'delegate lifecycle: unavailable');
    const sendFailureProjection = JSON.stringify(await listEvents(client, sendFailed.id));
    assert.ok(sendFailureProjection.includes('503'), sendFailureProjection);
    assert.ok(!sendFailureProjection.includes('REMOTE-CHILD-'));
    pass('remote agent_run send 5xx fails closed after stable-id retries');

    console.log('DELEGATED REMOTE LIFECYCLE TS API E2E PASS.');
  } finally {
    if (server) await stopServer(server.server).catch(() => {});
    await upstream.close();
    await peer.close();
  }
}

main().catch((error) => {
  console.error('DELEGATED REMOTE LIFECYCLE TS API E2E FAIL:', error);
  process.exitCode = 1;
});
