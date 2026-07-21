// Real-process lifecycle coverage for a remote child Run created by `agent_run`.
//
// A deterministic HTTP peer is the only test double. The coordinator, Managed
// API, durable continuation, cancellation token, A2A adapter and parent/child Run
// projection are production code.

import assert from 'node:assert/strict';
import http, { type IncomingMessage, type ServerResponse } from 'node:http';
import Anthropic from '@anthropic-ai/sdk';
import { pass, realServerEnv, spawnServer, startUpstream, stopServer, waitForPort } from './harness.mjs';

const PORT = Number(process.env.E2E_PORT ?? 38214);
const BETAS = ['managed-agents-2026-04-01'];
const sleep = (ms: number): Promise<void> => new Promise((resolve) => setTimeout(resolve, ms));

type PeerState = {
  freshMessages: number;
  polls: string[];
  cancels: string[];
  resumes: Array<{ contextId?: string; text: string }>;
};

function task(id: string, contextId: string, state: string, text?: string): Record<string, unknown> {
  return {
    id,
    contextId,
    status: {
      state,
      ...(text
        ? {
            message: {
              messageId: `reply-${id}`,
              role: 'agent',
              parts: [{ text }],
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

      state.freshMessages += 1;
      switch (state.freshMessages) {
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

    if (request.method === 'GET' && route === '/.well-known/agent-card.json') {
      json(response, 200, {
        name: 'remote-researcher',
        description: 'delegated lifecycle peer',
        url: 'http://127.0.0.1',
        version: '1',
        capabilities: {},
        defaultInputModes: ['text'],
        defaultOutputModes: ['text'],
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
    close: () => new Promise<void>((resolve) => server.close(() => resolve())),
  };
}

async function listEvents(client: Anthropic, sessionId: string): Promise<any[]> {
  const events: any[] = [];
  for await (const event of client.beta.sessions.events.list(sessionId, { betas: BETAS })) events.push(event);
  return events;
}

async function createSession(client: Anthropic): Promise<any> {
  return client.beta.sessions.create({ agent: 'assistant', environment_id: 'env_local', betas: BETAS });
}

async function sendText(client: Anthropic, sessionId: string, text: string): Promise<void> {
  await client.beta.sessions.events.send(sessionId, {
    events: [{ type: 'user.message', content: [{ type: 'text', text }] }],
    betas: BETAS,
  });
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
  const server = spawnServer('delegate-remote', PORT, {
    AWAKEN_REMOTE_AGENT_URL: peer.endpoint,
    ...realServerEnv('delegating', upstream, { mode: 'delegate-remote' }),
  });
  await waitForPort(PORT, 180_000, server.server);
  const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: `http://127.0.0.1:${PORT}` });
  try {
    // A remote child may pause for user input. The parent exposes an ordinary
    // client-executed ticket and resumes the exact task/context, not a new child.
    const awaiting = await createSession(client);
    await sendText(client, awaiting.id, 'delegate and wait for remote input');
    const awaitingEvents = await listEvents(client, awaiting.id);
    const toolUse = awaitingEvents.find(
      (event) => event.type === 'agent.custom_tool_use' && event.name === 'agent_run',
    );
    assert.ok(toolUse, `agent_run was projected: ${awaitingEvents.map((event) => event.type)}`);
    await client.beta.sessions.events.send(awaiting.id, {
      events: [
        {
          type: 'user.custom_tool_result',
          custom_tool_use_id: toolUse.id,
          content: [{ type: 'text', text: 'src/lib.rs' }],
          is_error: false,
        },
      ],
      betas: BETAS,
    });
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
    await client.beta.sessions.events.send(cancelled.id, {
      events: [{ type: 'user.interrupt' }],
      betas: BETAS,
    });
    await activeTurn.catch(() => {});
    await waitFor(() => peer.state.cancels.find((id) => id === 'delegated-cancel'), 'remote child cancellation');
    assert.deepEqual(peer.state.cancels, ['delegated-cancel']);
    pass('parent interrupt cancels the pinned remote child task exactly once');

    // A retryable remote 5xx remains an error outcome; no synthetic delegate
    // result is fed to the coordinator.
    const failed = await createSession(client);
    await assert.rejects(
      sendText(client, failed.id, 'delegate to unavailable remote child'),
      (error: any) => error?.status === 500 && String(error?.message).includes('503'),
    );
    const failedEvents = await listEvents(client, failed.id);
    const failedProjection = JSON.stringify(failedEvents);
    assert.ok(!failedProjection.includes('REMOTE-CHILD-'));
    pass('remote agent_run 5xx fails closed without a fabricated child result');

    console.log('DELEGATED REMOTE LIFECYCLE TS API E2E PASS.');
  } finally {
    await stopServer(server.server).catch(() => {});
    upstream.close();
    await peer.close();
  }
}

main().catch((error) => {
  console.error('DELEGATED REMOTE LIFECYCLE TS API E2E FAIL:', error);
  process.exitCode = 1;
});
