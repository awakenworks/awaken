// Real-process lifecycle coverage for a remote child Thread created through
// the Managed coordinator's fixed `list_agents` / `send_message` surface.
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
  waitForSessionEventReceipt,
  waitForValue,
} from './harness.mjs';
import { closeHttpServer } from './http_server.mjs';

const PORT = Number(process.env.E2E_PORT ?? 38214);
const BETAS = ['managed-agents-2026-04-01'];
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

async function sendText(client: Anthropic, sessionId: string, text: string): Promise<any> {
  return within(
    client.beta.sessions.events.send(sessionId, {
      events: [{ type: 'user.message', content: [{ type: 'text', text }] }],
      betas: BETAS,
    }),
    `Run for ${text}`,
  );
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
    // C1 send_message reaches remote input-required; C2 the frozen child Agent
    // does not declare `agent_input` as a custom client tool; C3 the aggregate
    // requires_action names the qualified child Event. Effects: E1 the public
    // Session list contains that exact `agent.tool_use` (allow) rather than an
    // orphan id; E2 the official generic `user.tool_result` resumes the pinned
    // task context once. Rule R1 C1+C2+C3=>E1+E2. The executed parent call must
    // never be mistaken for the later remote-input ticket.
    const awaiting = await createSession(client);
    const awaitingReceipt = (await sendText(
      client,
      awaiting.id,
      'delegate and wait for remote input',
    )).data[0];
    // Receipt rule R2: C4 the delegated command has an exact receipt; E3 its
    // processed delta contains the child Thread input-required boundary while
    // the independently completed root Thread may end_turn; K1 earlier history
    // cannot satisfy E3. Decision R2=C1+C4=>E1+E3 via the canonical observer.
    const { delta: awaitingEvents } = await waitForSessionEventReceipt(
      client,
      awaiting.id,
      awaitingReceipt.id,
      BETAS,
      ({ delta }: { delta: any[] }) => delta.some(
        (event: any) => event.type === 'session.thread_status_idle'
          && event.stop_reason?.type === 'requires_action',
      ),
      'remote child requires_action boundary',
      { timeoutMs: 30_000 },
    );
    const idle = awaitingEvents.find(
      (event: any) => event.type === 'session.thread_status_idle'
        && event.stop_reason?.type === 'requires_action',
    );
    const pendingId = idle?.stop_reason?.event_ids?.[0];
    const toolUse = awaitingEvents.find(
      (event: any) => event.id === pendingId && event.type === 'agent.tool_use' && event.name === 'agent_input',
    );
    assert.ok(toolUse, `remote agent_input was projected: ${JSON.stringify(awaitingEvents)}`);
    const resumeResponse = await within(client.beta.sessions.events.send(awaiting.id, {
      events: [
        {
          type: 'user.tool_result',
          tool_use_id: toolUse.id,
          content: [{ type: 'text', text: 'src/lib.rs' }],
          is_error: false,
        },
      ],
      betas: BETAS,
    }), 'remote child input resume');
    const resumeReceipt = resumeResponse?.data?.[0];
    assert.ok(resumeReceipt, 'remote child input resume returns an exact receipt');
    // Receipt rule R3: C5 exact generic tool-result receipt; E4 pinned child
    // resumes once and reaches its Thread boundary; K2 pre-resume events are
    // excluded. Decision R3=C2+C5=>E2+E4.
    const { delta: resumedEvents } = await waitForSessionEventReceipt(
      client,
      awaiting.id,
      resumeReceipt.id,
      BETAS,
      ({ delta }: { delta: any[] }) => JSON.stringify(delta).includes('REMOTE-CHILD-RESUMED')
        && delta.some((event: any) => event.type === 'session.thread_status_idle'),
      'remote child resumed marker and terminal Thread boundary',
      { timeoutMs: 30_000 },
    );
    assert.ok(JSON.stringify(resumedEvents).includes('REMOTE-CHILD-RESUMED'));
    assert.deepEqual(peer.state.resumes, [{ contextId: 'delegated-input-context', text: 'src/lib.rs' }]);
    pass('remote coordinated child input-required resumes on the pinned task context');

    // Active remote children share the common A2A poll driver.
    const polled = await createSession(client);
    const polledReceipt = (await sendText(
      client,
      polled.id,
      'delegate and poll remote child',
    )).data[0];
    // Poll rule P1: C1 exact command receipt plus working->completed remote task;
    // E1 receipt processed and terminal child marker committed; K1 old history is
    // excluded. Decision P1=C1=>E1 through the canonical receipt observer.
    const { delta: polledEvents } = await waitForSessionEventReceipt(
      client,
      polled.id,
      polledReceipt.id,
      BETAS,
      ({ delta }: { delta: any[] }) => JSON.stringify(delta).includes('REMOTE-CHILD-POLLED')
        && delta.some((event: any) => event.type === 'session.thread_status_idle'),
      'remote polled marker and terminal Thread boundary',
      { timeoutMs: 30_000 },
    );
    assert.ok(JSON.stringify(polledEvents).includes('REMOTE-CHILD-POLLED'));
    assert.ok(peer.state.polls.includes('delegated-polled'));
    pass('remote coordinated child polls a working task to its terminal result');

    // Interrupt the parent while its child is polling. Cancellation must cross
    // the same token into A2A and address the pinned child task once.
    const cancelled = await createSession(client);
    const activeRun = sendText(client, cancelled.id, 'delegate then interrupt remote child');
    await waitForValue(
      () => [...peer.state.polls],
      (polls: string[]) => polls.includes('delegated-cancel'),
      'remote child poll',
    );
    const interruptResponse = await within(client.beta.sessions.events.send(cancelled.id, {
      events: [{ type: 'user.interrupt' }],
      betas: BETAS,
    }), 'parent interrupt');
    const interruptReceipt = interruptResponse?.data?.[0];
    assert.ok(interruptReceipt, 'parent interrupt returns an exact receipt');
    const activeResponse = await within(activeRun, 'interrupted parent Run');
    const activeReceipt = activeResponse?.data?.[0];
    assert.equal(activeReceipt?.type, 'user.message', 'parent Run returns its exact User receipt');
    try {
      await waitForValue(
        () => [...peer.state.cancels],
        (cancels: string[]) => cancels.includes('delegated-cancel'),
        'remote child cancellation',
      );
    } catch (error) {
      const committed = await listEvents(client, cancelled.id);
      throw new Error(
        `${String(error)}; peer=${JSON.stringify(peer.state)}; committed=${JSON.stringify(committed)}`,
        { cause: error },
      );
    }
    // Interrupt rule I1: C1 exact parent User receipt, C2 exact interrupt receipt,
    // and C3 peer cancel delivery; E1 both receipts process plus the pinned remote
    // cancellation. K1 peer state is the external side-effect oracle; canonical
    // history owns only C1/C2 completion. D1=C1+C2+C3=>E1.
    await waitForSessionEventReceipt(
      client,
      cancelled.id,
      interruptReceipt.id,
      BETAS,
      () => true,
      'parent interrupt receipt to process after peer cancellation',
      { timeoutMs: 30_000 },
    );
    await waitForSessionEventReceipt(
      client,
      cancelled.id,
      activeReceipt.id,
      BETAS,
      () => true,
      'interrupted parent User receipt to process after peer cancellation',
      { timeoutMs: 30_000 },
    );
    assert.deepEqual(peer.state.cancels, ['delegated-cancel']);
    pass('parent interrupt cancels the pinned remote child task exactly once');

    // Failure decision rules: C1 a committed working task's poll returns 5xx;
    // C2 the initial send returns 5xx; C3 the remote task itself terminates
    // failed/rejected; C4 it terminates canceled. Effects: E1 C1 keeps the
    // Session running and the exact task eligible for durable reattachment; E2
    // C2/C3 commits the exact public error; E3 no rule fabricates REMOTE-CHILD
    // success; E4 C4 settles the child idle without manufacturing an error or
    // report. The SDK error shape owns a human-readable message, not the neutral
    // internal failure code.
    // Constraint: ADR-0057 makes poll/cancel delivery failure retryable, while a
    // task terminal or a rejected initial send is a completed attempt boundary.
    // R1 C1=>E1+E3; R2 C2=>E2+E3; R3 C3=>E2+E3; R4 C4=>E3+E4. Cold
    // reattachment of R1's exact task id is owned by remote_attempt_lifecycle_e2e.ts.
    const pollFailed = await createSession(client);
    await sendText(client, pollFailed.id, 'delegate lifecycle: poll failure');
    // R1 is intentionally state-only: C1 leaves the exact remote task running;
    // E1 is retry eligibility, not command completion. K1 therefore forbids a
    // processed-receipt/terminal predicate; D1 observes Session+peer state only.
    const retryablePoll = await waitForValue(
      async () => ({
        session: await client.beta.sessions.retrieve(pollFailed.id, { betas: BETAS }),
        events: await listEvents(client, pollFailed.id),
        polls: [...peer.state.polls],
      }),
      (observed: { session: any; polls: string[] }) => observed.session.status === 'running'
        && observed.polls.includes('delegated-poll-failure'),
      'retryable remote poll failure',
    );
    const pollFailureProjection = JSON.stringify(retryablePoll.events);
    assert.equal(retryablePoll.session.status, 'running', 'R1/E1');
    assert.ok(
      retryablePoll.polls.includes('delegated-poll-failure'),
      'R1 retains and polls the exact committed task id',
    );
    assert.ok(
      !pollFailureProjection.includes('503'),
      `R1 must not turn a retryable poll delivery failure into terminal truth: ${pollFailureProjection}`,
    );
    assert.ok(!pollFailureProjection.includes('REMOTE-CHILD-'), 'R1/E3');
    pass('remote coordinated child poll 5xx remains durably retryable without fabricated output');

    for (const [prompt, message] of [
      ['delegate lifecycle: failed', 'remote A2A task ended in the failed state'],
      ['delegate lifecycle: rejected', 'remote A2A task ended in the rejected state'],
    ]) {
      const failed = await createSession(client);
      const failedReceipt = (await sendText(client, failed.id, prompt)).data[0];
      // Failure rule F1: C1 exact command receipt plus a failed/rejected remote
      // terminal; E1 processed receipt and exact Session error; K1 no earlier
      // error may satisfy this case. Decision F1=C1=>E1.
      const { delta: failedEvents } = await waitForSessionEventReceipt(
        client,
        failed.id,
        failedReceipt.id,
        BETAS,
        ({ delta }: { delta: any[] }) => delta.some(
          (event: any) => event.type === 'session.error' && event.error?.message === message,
        ),
        `${prompt} projection`,
        { timeoutMs: 30_000 },
      );
      const failedProjection = JSON.stringify(failedEvents);
      assert.ok(failedProjection.includes(message), `${prompt}: ${failedProjection}`);
      assert.ok(!failedProjection.includes('REMOTE-CHILD-'), prompt);
    }
    pass('remote failed/rejected terminals remain explicit Session errors');

    const remotelyCanceled = await createSession(client);
    const canceledReceipt = (await sendText(
      client,
      remotelyCanceled.id,
      'delegate lifecycle: canceled',
    )).data[0];
    // Cancel-terminal rule C1: exact command receipt plus remote canceled state;
    // E1 processed receipt and idle child/Session without output; K1 prior idle
    // history is excluded. Decision C1=>E1.
    const { delta: canceledEvents } = await waitForSessionEventReceipt(
      client,
      remotelyCanceled.id,
      canceledReceipt.id,
      BETAS,
      ({ delta }: { delta: any[] }) => delta.some(
        (event: any) => event.type === 'session.thread_status_idle'
          && event.agent_name === 'researcher',
      ) && delta.some((event: any) => event.type === 'session.status_idle'),
      'remote canceled child settlement',
      { timeoutMs: 30_000 },
    );
    const canceledProjection = JSON.stringify(canceledEvents);
    assert.ok(!canceledProjection.includes('session.error'), 'R4/E4');
    assert.ok(!canceledProjection.includes('REMOTE-CHILD-'), 'R4/E3');
    pass('remote canceled terminal settles without a fabricated child result');

    const sendFailed = await createSession(client);
    const sendFailedReceipt = (await sendText(
      client,
      sendFailed.id,
      'delegate lifecycle: unavailable',
    )).data[0];
    // Send-failure rule S1: C1 exact command receipt plus stable-id retries
    // exhausted; E1 processed receipt and public 503 diagnostic; K1 older errors
    // are excluded. Decision S1=C1=>E1.
    const sendFailureProjection = JSON.stringify((await waitForSessionEventReceipt(
      client,
      sendFailed.id,
      sendFailedReceipt.id,
      BETAS,
      ({ delta }: { delta: any[] }) => JSON.stringify(delta).includes('503'),
      'remote send failure projection',
      { timeoutMs: 30_000 },
    )).delta);
    assert.ok(sendFailureProjection.includes('503'), sendFailureProjection);
    assert.ok(!sendFailureProjection.includes('REMOTE-CHILD-'));
    pass('remote coordinated child send 5xx fails closed after stable-id retries');

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
