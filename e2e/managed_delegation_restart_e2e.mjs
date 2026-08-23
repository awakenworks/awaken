// Durable coordinated child-Thread recovery through the official Managed Agents
// TypeScript SDK. The fixed send command creates ordinary persisted Thread/Run
// state; a replacement server rebuilds its disposable protocol projection from
// that same runtime truth, without a second Subagent aggregate.
//
// Cause graph:
//   list_agents -> accepted send_to_agent -> completed child Thread/Run
//   -> tool-free coordinator report Run -> process loss
//   -> runtime Thread relationship read -> typed projection
//   -> ExistingThread follow-up -> deterministic fresh Run on the same Thread
//   -> retained child transcript reaches the replacement process's model request
//   missing/corrupt Thread relationship -X-> invented child Thread
//
// Decision table (causes: relationship/history present, process original/replaced,
// target existing/absent; effects: same Thread, fresh ordered Run, retained
// history, or fail-closed absence):
// | Rule | Durable relationship/history | Process | Target | Effect |
// |---|---|---|---|---|
// | R1 | accepted send + completed child + report ack | original | created | one idle child; no repeated send |
// | R2 | accepted send + completed child | replacement | existing | same id/agent/status/parent/history |
// | R3 | R2 | replacement | ExistingThread follow-up | no new Thread; fresh Run sees retained history; one report |
// | R4 | absent | either | existing/created | primary only; no invented child |
// | R5 | completed Advisor consultation | replacement | existing | same Advisor Thread and exact Run-derived event identity; no replayed inference |

// Constraints/invariant: runtime Thread/Run relationships and committed events
// remain the only durable authority; restart may rebuild projection, never a
// parallel child aggregate. Effects are the R1-R5 table outcomes.

// Run: node e2e/managed_delegation_restart_e2e.mjs

import assert from 'node:assert/strict';
import fs from 'node:fs';
import Anthropic from '@anthropic-ai/sdk';
import {
  pass,
  realServerEnv,
  spawnServer,
  startUpstream,
  stopServer,
  waitForPort,
  waitForSessionEventReceipt,
  waitForValue,
} from './harness.mjs';

const PORT = Number(process.env.E2E_PORT ?? 38237);
const BETAS = ['managed-agents-2026-04-01'];
const STORE_DIR = `/tmp/awaken-delegation-restart-e2e-${process.pid}`;

async function listThreads(client, sessionId) {
  const threads = [];
  for await (const thread of client.beta.sessions.threads.list(sessionId, { betas: BETAS })) {
    threads.push(thread);
  }
  return threads;
}

async function listThreadEvents(client, sessionId, threadId) {
  const events = [];
  for await (const event of client.beta.sessions.threads.events.list(threadId, {
    session_id: sessionId,
    betas: BETAS,
  })) events.push(event);
  return events;
}

async function listSessionEvents(client, sessionId) {
  const events = [];
  for await (const event of client.beta.sessions.events.list(sessionId, { betas: BETAS })) {
    events.push(event);
  }
  return events;
}

async function waitForIdle(client, sessionId, acceptedId) {
  assert.equal(typeof acceptedId, 'string', 'official SDK returns the accepted User Event id');
  // Receipt/settlement cause-effect rule W1: C1=history may contain an older
  // idle edge; C2=this exact receipt is accepted; C3=its Run later settles.
  // C1+C2 without C3 keeps polling; C2+C3 returns only after the receipt is
  // processed and a new idle edge after it commits. This prevents restart and
  // follow-up checks from reusing pre-existing terminal history.
  const settled = await waitForSessionEventReceipt(
    client,
    sessionId,
    acceptedId,
    BETAS,
    async ({ delta }) => {
      const session = await client.beta.sessions.retrieve(sessionId, { betas: BETAS });
      if (session.status === 'terminated') {
        throw new Error(`Session ${sessionId} terminated during coordination`);
      }
      return session.status === 'idle'
        && delta.some((event) => event.type === 'session.status_idle');
    },
    `Session ${sessionId} did not settle after coordinated child work`,
    { timeoutMs: 60_000, pollMs: 200 },
  );
  return settled.events;
}

async function waitForAdvisor(client, sessionId) {
  const threads = await waitForValue(
    () => listThreads(client, sessionId),
    (listed) => listed.some(
      (thread) => thread.agent?.type === 'advisor' && thread.status === 'terminated',
    ),
    `Session ${sessionId} did not project a terminal Advisor Thread`,
    { timeoutMs: 30_000 },
  );
  const advisor = threads.find(
    (thread) => thread.agent?.type === 'advisor' && thread.status === 'terminated',
  );
  return { advisor, events: await listThreadEvents(client, sessionId, advisor.id) };
}

async function main() {
  fs.rmSync(STORE_DIR, { recursive: true, force: true });
  fs.mkdirSync(STORE_DIR, { recursive: true });
  const upstream = await startUpstream('delegating');
  const environment = {
    SESSION_DEPLOYMENT_STORAGE_DIR: STORE_DIR,
    ...realServerEnv('delegating', upstream, { mode: 'delegate' }),
  };
  let server;
  try {
    server = spawnServer('delegate', PORT, environment);
    await waitForPort(PORT);
    let client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: server.baseUrl });
    const session = await client.beta.sessions.create({
      agent: 'assistant',
      environment_id: 'env_local',
      betas: BETAS,
    });
    const researchReceipt = await client.beta.sessions.events.send(session.id, {
      events: [{ type: 'user.message', content: [{ type: 'text', text: 'research the answer' }] }],
      betas: BETAS,
    });
    await waitForIdle(client, session.id, researchReceipt.data[0]?.id);
    const beforeEvents = await listSessionEvents(client, session.id);
    assert.deepEqual(
      beforeEvents
        .filter((event) => event.type === 'agent.tool_use')
        .map((event) => event.name),
      ['list_agents', 'send_to_agent'],
      'the original process commits exactly one fixed coordination sequence',
    );
    const coordinatorMessages = beforeEvents
      .filter((event) => event.type === 'agent.message')
      .flatMap((event) => event.content ?? [])
      .map((block) => block.text ?? '');
    assert.ok(
      coordinatorMessages.some((text) => text.includes('coordination accepted:')),
      `the first Run ends on the asynchronous send receipt: ${coordinatorMessages}`,
    );
    assert.equal(
      coordinatorMessages.filter((text) => text === 'coordination completed from child report').length,
      1,
      `the later report Run commits once before restart: ${coordinatorMessages}`,
    );
    const before = await listThreads(client, session.id);
    const original = before.find((thread) => thread.parent_thread_id !== null);
    assert.ok(original, 'the committed delegation creates one child Thread');
    assert.equal(original.status, 'idle');

    // R5 cause/effect: a successful reserved Advisor call commits one ordinary
    // child Thread/Run with an exactly paired model span before process loss;
    // replacement projection must retain the same Thread id and the same
    // Run-derived event ids without another Provider request. The existing
    // Thread/Run store and projector are the only authorities; no Advisor
    // recovery registry is added.
    const advisorSession = await client.beta.sessions.create({
      agent: 'assistant',
      environment_id: 'env_local',
      betas: BETAS,
    });
    const advisorReceipt = await client.beta.sessions.events.send(advisorSession.id, {
      events: [{
        type: 'user.message',
        content: [{ type: 'text', text: 'consult the advisor before restart' }],
      }],
      betas: BETAS,
    });
    await waitForIdle(client, advisorSession.id, advisorReceipt.data[0]?.id);
    const advisorBefore = await waitForAdvisor(client, advisorSession.id);
    assert.deepEqual(
      advisorBefore.advisor.agent,
      { type: 'advisor', model: 'claude-opus-4-8' },
      'R5 the original process projects the exact frozen Advisor identity',
    );
    const advisorSignature = advisorBefore.events.map((event) => [event.type, event.id]);
    assert.deepEqual(
      advisorBefore.events.map((event) => event.type),
      [
        'session.thread_status_running',
        'span.model_request_start',
        'span.model_request_end',
        'session.thread_status_idle',
        'session.thread_status_terminated',
      ],
      'R5 the original process commits the isolated Advisor model/lifecycle while delivery stays root-only',
    );
    const advisorModelStart = advisorBefore.events.find(
      (event) => event.type === 'span.model_request_start',
    );
    const advisorModelEnd = advisorBefore.events.find(
      (event) => event.type === 'span.model_request_end',
    );
    assert.equal(
      advisorModelEnd.model_request_start_id,
      advisorModelStart.id,
      'R5 ordinary Advisor child inference owns one exactly paired model span',
    );
    const requestsBeforeRestart = upstream.requests.length;

    await stopServer(server.server);
    server = spawnServer('delegate', PORT, environment);
    await waitForPort(PORT);
    client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: server.baseUrl });
    const after = await listThreads(client, session.id);
    const restored = after.find((thread) => thread.parent_thread_id !== null);
    assert.ok(restored, 'the replacement process rebuilds the child projection');
    assert.equal(restored.id, original.id, 'exact durable child Run id is preserved');
    assert.equal(restored.agent.id, original.agent.id, 'exact delegate identity is preserved');
    assert.equal(restored.parent_thread_id, original.parent_thread_id);
    assert.equal(restored.status, 'idle', 'completed relationship restores as idle');
    const restoredEvents = await listThreadEvents(client, session.id, restored.id);
    // R2 model-observation cause/effect: the committed child Run owns one model
    // request start/end pair; cold projection must rebuild those exact events in
    // order with the transcript, rather than dropping or duplicating them.
    assert.deepEqual(
      restoredEvents.map((event) => event.type),
      [
        'session.thread_status_running',
        'agent.thread_message_received',
        'span.model_request_start',
        'span.model_request_end',
        'agent.thread_message_sent',
        'session.thread_status_idle',
      ],
      'replacement process rebuilds the child-perspective history through the same projector',
    );
    const restoredInput = restoredEvents.find((event) => event.type === 'agent.thread_message_received');
    assert.ok(
      restoredInput.content.some((block) => block.text === 'do the research'),
      'the recovered child input comes from committed send_to_agent message',
    );
    const restoredReply = restoredEvents.find((event) => event.type === 'agent.thread_message_sent');
    assert.ok(
      restoredReply.content.some((block) => block.text?.includes('researched: 42')),
      'the recovered child reply is projected as a message sent to the parent',
    );

    const advisorAfter = await waitForAdvisor(client, advisorSession.id);
    assert.equal(
      advisorAfter.advisor.id,
      advisorBefore.advisor.id,
      'R5 replacement process retains the exact Advisor Thread id',
    );
    assert.deepEqual(
      advisorAfter.events.map((event) => [event.type, event.id]),
      advisorSignature,
      'R5 replacement rebuilds the same Run-derived lifecycle/event identities',
    );
    assert.equal(
      upstream.requests.length,
      requestsBeforeRestart,
      'R5 cold projection does not replay the Advisor inference',
    );

    const followUpReceipt = await client.beta.sessions.events.send(session.id, {
      events: [{ type: 'user.message', content: [{ type: 'text', text: 'follow up with the same child' }] }],
      betas: BETAS,
    });
    const followUpEvents = await waitForIdle(client, session.id, followUpReceipt.data[0]?.id);
    assert.deepEqual(
      followUpEvents
        .filter((event) => event.type === 'agent.tool_use')
        .map((event) => event.name),
      ['list_agents', 'send_to_agent', 'list_agents', 'send_to_agent'],
      'R3 the replacement uses the same fixed ExistingThread coordination path',
    );
    const afterFollowUp = await listThreads(client, session.id);
    assert.equal(afterFollowUp.length, 2, 'R3 no replacement child Thread is invented');
    assert.equal(
      afterFollowUp.find((thread) => thread.parent_thread_id !== null)?.id,
      original.id,
      'R3 the exact durable child Thread receives the follow-up',
    );
    const followedUpHistory = await listThreadEvents(client, session.id, original.id);
    assert.deepEqual(
      followedUpHistory.map((event) => event.type),
      [
        'session.thread_status_running',
        'agent.thread_message_received',
        'span.model_request_start',
        'span.model_request_end',
        'agent.thread_message_sent',
        'session.thread_status_idle',
        'session.thread_status_running',
        'agent.thread_message_received',
        'span.model_request_start',
        'span.model_request_end',
        'agent.thread_message_sent',
        'session.thread_status_idle',
      ],
      'R3 one deterministic fresh Run follows the restored Run on the same Thread',
    );
    const followedUpReply = followedUpHistory
      .filter((event) => event.type === 'agent.thread_message_sent')
      .at(-1);
    assert.ok(
      followedUpReply.content.some((block) =>
        block.text?.includes('follow-up retained researched: 42'),
      ),
      'R3 the replacement child model receives its pre-restart committed history',
    );
    pass('child Thread projection survived a real process restart from Thread/Run truth');
    console.log('E2E PASS: durable Managed child Thread projection, follow-up, and history recovery.');
  } finally {
    if (server) await stopServer(server.server);
    upstream.close();
    fs.rmSync(STORE_DIR, { recursive: true, force: true });
  }
}

main().catch((error) => {
  console.error('E2E FAIL:', error);
  process.exitCode = 1;
});
