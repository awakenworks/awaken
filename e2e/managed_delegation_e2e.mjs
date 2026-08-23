// Managed multi-Agent coordination end-to-end with the official Anthropic TS
// SDK. The coordinator uses the fixed `list_agents` and `send_to_agent` tools.
// The send result is an admission receipt; Native/ACP child work runs on an
// ordinary child Thread and returns through a cross-Thread event, asynchronously
// from the coordinator's final message.
//
// Uses the delegation server (AWAKEN_MODEL_MODE=delegate): roster = {researcher};
// `ghost` is intentionally absent so the fail-closed path can be shown too.
//
// Run: (from e2e/)  npm install && node managed_delegation_e2e.mjs
//
// Cause/effect graph:
//   frozen roster + fixed tools -> list_agents result -> accepted send_to_agent
//   -> stable child Thread + fresh child Run -> asynchronous cross-Thread reply
//   -> one report Run with no second send -> child-perspective projection
//   -> targeted interrupt -> archive termination
//   unlisted target -> rejected send result -X-> child Thread / child inference
//   send receipt -X-> synchronous child payload in the coordinator message
//
// Decision table:
// | rule | target | fixed sequence | child backend | effect |
// |---|---|---|---|---|
// | M1 | Native roster member | list -> accepted send | Native | receipt, child reply, then one tool-free report acknowledgement |
// | M2 | explicit self | list -> accepted send | frozen owner copy | isolated child answers without recursive fan-out |
// | M3 | ACP roster member | list -> accepted send | ACP | external result cross-posts from ordinary child Thread |
// | M4 | absent member | list -> rejected send | none | no child and no child inference usage |
// | M5 | idle child | targeted interrupt | existing | only selected child stream receives receipt |
// | M6 | idle child | archive twice | existing | one terminal transition; repeat is idempotent |
// | M7 | one classified + one withheld child call | targeted interrupt | Native | exact ask first; cancellation classifies the sibling and both results once; child end_turn; no child/root resample |
// | M8 | one classified + one withheld child call | global interrupt | Native | same occurrence-scoped terminal effects through fan-out selection |
// | M9 | one classified + one withheld child call | archive once | Native | cancellation classifies both calls, then one terminated transition |
// | M10 | requires_action child | wrong/duplicate confirmation batch | Native | whole request is 400; no receipt, effect, or sampling |
// | M11 | requires_action child | exact confirmations allow then deny | Native | same Thread/Run; committed permission tickets resolve in order, then one child continuation and one root report |
// | M12 | reserved Advisor | successful terminal child | Native pinned model | isolated Advisor Thread returns one advice block to root |
// | M13 | reserved Advisor | provider failure | Native pinned model | generic root failure; no partial/provider detail on child or root |
// | M14 | reserved Advisor | targeted cancel while Running | Native pinned model | child terminates; no partial advice crosses the Thread boundary |
//
// Constraints/invariant: the frozen roster, ordinary child Thread/Run, and
// committed cross-Thread report are the only authorities; send receipts are
// admission facts and never synchronous child results.
// Effects are the M1-M14 table outcomes; each rule observes both child and root
// projections so an unrelated Thread cannot satisfy the case.
//
// FMECA D6: accidentally awaiting the child as a tool result collapses the
// durable asynchronous boundary and can deadlock/recurse on the parent task.
// M1 proves the receipt and child reply are distinct observations; M5/M6 prove
// later control addresses the durable child rather than an inline call frame.
// M7-M9 additionally prove cancellation provenance crosses the existing claimed
// dispatch boundary: terminal text is never parsed to decide whether to report.

import assert from 'node:assert/strict';
import Anthropic from '@anthropic-ai/sdk';
import {
  waitForSessionEventReceipt,
  waitForValue,
  withScenarioServer,
} from './harness.mjs';
import { FAKE_USAGE } from './fixtures/fake_anthropic_fixture.mjs';

// The provider adapter (genai) normalizes input to the TOTAL input incl. the
// prompt-cache tokens, so each inference reports this as `input_tokens`.
const PER_INFERENCE_INPUT =
  FAKE_USAGE.input_tokens + FAKE_USAGE.cache_read_input_tokens + FAKE_USAGE.cache_creation_input_tokens;

const PORT = Number(process.env.E2E_PORT ?? 38105);
const BETAS = ['managed-agents-2026-04-01'];

async function listEvents(client, sessionId) {
  const events = [];
  for await (const ev of client.beta.sessions.events.list(sessionId, { betas: BETAS })) events.push(ev);
  return events;
}

async function listThreadEvents(client, sessionId, threadId) {
  const events = [];
  for await (const event of client.beta.sessions.threads.events.list(threadId, {
    session_id: sessionId,
    betas: BETAS,
  })) events.push(event);
  return events;
}

async function listThreads(client, sessionId) {
  const threads = [];
  for await (const thread of client.beta.sessions.threads.list(sessionId, { betas: BETAS })) {
    threads.push(thread);
  }
  return threads;
}

async function statusOf(promise, label) {
  try {
    await promise;
  } catch (error) {
    if (typeof error?.status === 'number') return error.status;
    throw new Error(`${label}: non-HTTP failure: ${error}`);
  }
  throw new Error(`${label}: expected an HTTP failure`);
}

async function createAwaitingChild(client, prompt) {
  const session = await client.beta.sessions.create({
    agent: 'assistant',
    environment_id: 'env_local',
    betas: BETAS,
  });
  const receipt = (await client.beta.sessions.events.send(session.id, {
    events: [{ type: 'user.message', content: [{ type: 'text', text: prompt }] }],
    betas: BETAS,
  })).data[0];
  let boundary = null;
  // Awaiting-child receipt rule A1: C4 the creating command has an exact
  // receipt; E4 that receipt is processed with the occurrence-qualified child
  // requires_action boundary. K1 an older primary/child event cannot satisfy
  // E4. Decision A1=C1-C4=>E1-E4.
  await waitForSessionEventReceipt(
    client,
    session.id,
    receipt.id,
    BETAS,
    async ({ events: primaryEvents, delta }) => {
      const created = delta.find(
        (event) => event.type === 'session.thread_created' && event.agent_name === 'researcher',
      );
      if (!created) return false;
      const childEvents = await listThreadEvents(client, session.id, created.session_thread_id);
      const toolUses = childEvents.filter((event) => event.type === 'agent.tool_use');
      const idle = [...childEvents]
        .reverse()
        .find((event) => event.type === 'session.thread_status_idle');
      const pendingEventIds = idle?.stop_reason?.event_ids ?? [];
      // M7-M11 occurrence boundary: C1 the committed child Message contains an
      // ordered two-call ToolBatch; C2 exactly one ResumeTicket classifies the
      // current call; C3 its sibling has neither a ticket nor a result. E1 only
      // the exact ticket-qualified ask is appendable; E2 requires_action names
      // that same stable Event id; E3 the sibling stays absent until later
      // evidence classifies it. K: the committed Event log is the idempotency
      // ledger, so elapsed time and Message visibility cannot infer permission.
      // The receipt-scoped delta selects this Run's effect, while the retained
      // primary history is the no-op baseline for later rejected batches.
      if (
        toolUses.length !== 1
        || idle?.stop_reason?.type !== 'requires_action'
        || pendingEventIds.length !== 1
        || toolUses[0].id !== pendingEventIds[0]
      ) return false;
      boundary = {
        session,
        childId: created.session_thread_id,
        primaryEvents,
        childEvents,
        toolUses,
        idle,
        pendingEventId: pendingEventIds[0],
      };
      return true;
    },
    'coordinated child did not reach the exact requires_action boundary',
    { timeoutMs: 30_000 },
  );
  return boundary;
}

async function waitForSettledChild(client, sessionId, childId, label = 'child end_turn and aggregate idle') {
  return waitForValue(
    async () => {
      const [session, primaryEvents, childEvents] = await Promise.all([
        client.beta.sessions.retrieve(sessionId, { betas: BETAS }),
        listEvents(client, sessionId),
        listThreadEvents(client, sessionId, childId),
      ]);
      const idle = [...childEvents]
        .reverse()
        .find((event) => event.type === 'session.thread_status_idle');
      if (session.status !== 'idle' || idle?.stop_reason?.type !== 'end_turn') return null;
      return { primaryEvents, childEvents };
    },
    (value) => value !== null,
    label,
    { timeoutMs: 30_000 },
  );
}

async function waitForNextChildAction(client, sessionId, childId, priorIdleId) {
  return waitForValue(
    async () => {
      const childEvents = await listThreadEvents(client, sessionId, childId);
      const idle = [...childEvents]
        .reverse()
        .find((event) =>
          event.type === 'session.thread_status_idle'
          && event.stop_reason?.type === 'requires_action'
          && event.id !== priorIdleId);
      const pendingEventIds = idle?.stop_reason?.event_ids ?? [];
      if (pendingEventIds.length !== 1) return null;
      return { childEvents, idle, pendingEventId: pendingEventIds[0] };
    },
    (value) => value !== null,
    'child did not reach its next exact requires_action boundary',
    { timeoutMs: 30_000 },
  );
}

function assertInterruptedBatch(waiting, settled, requestsBeforeControl, requestsAfterControl, rule) {
  const INTERRUPTED = 'Tool execution was interrupted before completion. Please retry.';
  const toolUses = settled.childEvents.filter((event) => event.type === 'agent.tool_use');
  const results = settled.childEvents.filter((event) => event.type === 'agent.tool_result');
  assert.equal(toolUses.length, 2, `${rule}: later cancellation evidence adds the withheld sibling once`);
  assert.equal(
    toolUses.filter((toolUse) => toolUse.id === waiting.pendingEventId).length,
    1,
    `${rule}: the first ticket-qualified ask is not duplicated`,
  );
  for (const event of waiting.childEvents.filter(
    (candidate) => candidate.type === 'agent.message' || candidate.type === 'agent.thinking',
  )) {
    assert.equal(
      settled.childEvents.filter((candidate) => candidate.id === event.id).length,
      1,
      `${rule}: already-projected child text remains exactly once`,
    );
  }
  assert.equal(results.length, toolUses.length, `${rule}: every classified call has one result`);
  assert.deepEqual(
    results.map((result) => result.tool_use_id),
    toolUses.map((toolUse) => toolUse.id),
    `${rule}: results preserve ToolBatch order and identity`,
  );
  assert.ok(results.every((result) => result.is_error === true), `${rule}: every result is an error`);
  assert.ok(
    results.every((result) => JSON.stringify(result.content).includes(INTERRUPTED)),
    `${rule}: every result carries the fixed interruption text`,
  );
  assert.equal(requestsAfterControl, requestsBeforeControl, `${rule}: no child or primary model resample`);
  assert.equal(
    settled.primaryEvents.filter((event) => event.type === 'session.status_running').length,
    waiting.primaryEvents.filter((event) => event.type === 'session.status_running').length,
    `${rule}: no deterministic primary report Run was admitted`,
  );
  assert.ok(
    !messages(settled.primaryEvents).includes('coordination completed from child report'),
    `${rule}: interruption does not synthesize a normal child report`,
  );
}

async function runSession(client, sessionId, text) {
  const receipt = await client.beta.sessions.events.send(sessionId, {
    events: [{ type: 'user.message', content: [{ type: 'text', text }] }],
    betas: BETAS,
  });
  const acceptedId = receipt.data[0]?.id;
  assert.equal(typeof acceptedId, 'string', 'official SDK returns the accepted User Event id');
  // `send_to_agent` is intentionally asynchronous: the HTTP send can return
  // after the admission receipt while the child still keeps the Session active.
  // Cause/effect rule RS1: C1=a Session may already expose an older idle edge;
  // C2=this send returns a new exact durable receipt; C3=its coordinated Run
  // later settles. E1=the helper cannot reuse C1, and E2=it returns only when
  // C2 is processed and a new idle edge after C2 proves C3. Decision table:
  // C1+C2 without C3 => keep polling; C2+C3 => return this Run's full history.
  const settled = await waitForSessionEventReceipt(
    client,
    sessionId,
    acceptedId,
    BETAS,
    async ({ delta }) => {
      const session = await client.beta.sessions.retrieve(sessionId, { betas: BETAS });
      if (session.status === 'terminated') {
        throw new Error(`Session ${sessionId} terminated while coordinating`);
      }
      return session.status === 'idle'
        && delta.some((event) => event.type === 'session.status_idle');
    },
    `Session ${sessionId} did not settle after coordinated child work`,
    { timeoutMs: 60_000, pollMs: 200 },
  );
  return settled.events;
}

function messages(events) {
  return events.filter((e) => e.type === 'agent.message').map((e) => e.content[0].text);
}

function toolNames(events) {
  return events.filter((event) => event.type === 'agent.tool_use').map((event) => event.name);
}

function receivedText(events, agentName) {
  return events
    .filter((event) => event.type === 'agent.thread_message_received' && event.from_agent_name === agentName)
    .flatMap((event) => event.content ?? [])
    .map((block) => block.text ?? '')
    .join('');
}

async function settledAdvisorEvidence(client, sessionId) {
  return waitForValue(
    async () => {
      const [session, events, threads] = await Promise.all([
        client.beta.sessions.retrieve(sessionId, { betas: BETAS }),
        listEvents(client, sessionId),
        listThreads(client, sessionId),
      ]);
      const advisor = threads.find((thread) => thread.agent?.type === 'advisor');
      if (!advisor || advisor.status !== 'terminated' || session.status !== 'idle') return null;
      const childEvents = await listThreadEvents(client, sessionId, advisor.id);
      if (!childEvents.some((event) => event.type === 'session.thread_status_terminated')) return null;
      return { session, events, threads, advisor, childEvents };
    },
    (value) => value !== null,
    'terminal Advisor Thread projection was not committed',
    { timeoutMs: 30_000 },
  );
}

async function main() {
  await withScenarioServer('delegate', 'delegating', PORT, async (baseUrl, upstream) => {
    const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: baseUrl });

    // M1: discover the roster, enqueue Native `researcher`, then observe its
    // reply on the child relationship rather than in the send receipt.
    const ok = await client.beta.sessions.create({ agent: 'assistant', environment_id: 'env_local', betas: BETAS });
    const okEvents = await runSession(client, ok.id, 'research the answer');
    assert.deepEqual(
      toolNames(okEvents),
      ['list_agents', 'send_to_agent'],
      `fixed Managed coordination sequence: ${toolNames(okEvents)}`,
    );
    assert.ok(
      messages(okEvents).some((message) => message.includes('coordination accepted:')),
      `coordinator ended on the admission receipt: ${messages(okEvents)}`,
    );
    assert.ok(
      !messages(okEvents).some((message) => message.includes('researched: 42')),
      `child payload is not a synchronous send result: ${messages(okEvents)}`,
    );
    const okIdle = [...okEvents].reverse().find((e) => e.type === 'session.status_idle');
    assert.equal(okIdle.stop_reason.type, 'end_turn');

    assert.equal(
      messages(okEvents).filter((message) => message === 'coordination completed from child report').length,
      1,
      `the normal terminal child admits exactly one later report Run: ${messages(okEvents)}`,
    );

    // M1 usage has 3 initial coordinator inferences (list, send, receipt), one
    // child inference, and one later tool-free report inference. Session usage
    // is the aggregate across all of those Runs and Threads.
    const INFERENCES = 5;
    const okUsage = (await client.beta.sessions.retrieve(ok.id, { betas: BETAS })).usage ?? {};
    assert.equal(okUsage.output_tokens, FAKE_USAGE.output_tokens * INFERENCES,
      `delegated usage folds in the sub-agent (output): ${JSON.stringify(okUsage)}`);
    assert.equal(okUsage.input_tokens, PER_INFERENCE_INPUT * INFERENCES,
      `delegated usage folds in the sub-agent (input): ${JSON.stringify(okUsage)}`);
    assert.equal(okUsage.cache_read_input_tokens, FAKE_USAGE.cache_read_input_tokens * INFERENCES,
      `delegated usage folds in the sub-agent (cache_read): ${JSON.stringify(okUsage)}`);
    assert.equal(okUsage.cache_creation?.ephemeral_5m_input_tokens, FAKE_USAGE.cache_creation_input_tokens * INFERENCES,
      `delegated usage folds in the sub-agent (cache_creation): ${JSON.stringify(okUsage)}`);

    // D4: the delegate call spawned a subagent child thread — announced by
    // `session.thread_created` and enumerable via the threads API.
    const created = okEvents.find((e) => e.type === 'session.thread_created');
    assert.ok(created, `expected session.thread_created, got: ${okEvents.map((e) => e.type)}`);
    assert.equal(created.agent_name, 'researcher');

    const threads = await listThreads(client, ok.id);
    assert.equal(threads.length, 2, `primary + researcher child: ${threads.map((t) => t.id)}`);
    const primary = threads.find((t) => t.parent_thread_id === null);
    const child = threads.find((t) => t.id === created.session_thread_id);
    assert.ok(child, 'the created child thread is enumerated');
    assert.equal(child.parent_thread_id, primary.id, 'child links to the primary thread');
    assert.equal(child.agent.name, 'researcher');

    const gotChild = await client.beta.sessions.threads.retrieve(child.id, {
      session_id: ok.id,
      betas: BETAS,
    });
    assert.equal(gotChild.id, child.id, 'the child thread is retrievable');

    // The asynchronous child Run projects its full lifecycle: created → running
    // → sent message → received reply → idle.
    const childEvents = okEvents.filter(
      (e) =>
        e.session_thread_id === child.id ||
        e.to_session_thread_id === child.id ||
        e.from_session_thread_id === child.id,
    );
    assert.deepEqual(
      childEvents.map((e) => e.type),
      [
        'session.thread_created',
        'session.thread_status_running',
        'agent.thread_message_sent',
        'agent.thread_message_received',
        'session.thread_status_idle',
      ],
      `child thread lifecycle: ${childEvents.map((e) => e.type)}`,
    );
    const sent = childEvents.find((e) => e.type === 'agent.thread_message_sent');
    assert.equal(sent.to_agent_name, 'researcher');
    const recv = childEvents.find((e) => e.type === 'agent.thread_message_received');
    assert.equal(recv.from_agent_name, 'researcher');
    assert.ok(
      recv.content.map((b) => b.text ?? '').join('').includes('researched: 42'),
      `the received reply carries the delegate output: ${JSON.stringify(recv.content)}`,
    );
    const childIdle = childEvents.find((e) => e.type === 'session.thread_status_idle');
    assert.equal(childIdle.stop_reason.type, 'end_turn');

    const ownChildEvents = [];
    for await (const event of client.beta.sessions.threads.events.list(child.id, {
      session_id: ok.id,
      betas: BETAS,
    })) ownChildEvents.push(event);
    // M1 model-observation cause/effect: one committed child inference (cause)
    // owns one paired start/end span on the child stream (effect), while the
    // cross-Thread reply remains the only message projection. Decision rule:
    // completed child inference => exact span pair + one sent reply + no
    // `agent.message` or aggregate Session status on the child view.
    assert.deepEqual(
      ownChildEvents.map((event) => event.type),
      [
        'session.thread_status_running',
        'agent.thread_message_received',
        'span.model_request_start',
        'span.model_request_end',
        'agent.thread_message_sent',
        'session.thread_status_idle',
      ],
      'a report-only child owns its model span pair and reverses cross-Thread messages without agent.message',
    );
    assert.equal(ownChildEvents[1].from_session_thread_id, primary.id);
    assert.equal(ownChildEvents[1].from_agent_name, undefined);
    const modelStart = ownChildEvents.find((event) => event.type === 'span.model_request_start');
    const modelEnd = ownChildEvents.find((event) => event.type === 'span.model_request_end');
    assert.equal(modelEnd.model_request_start_id, modelStart.id, 'M1 child model spans are exactly paired');
    const ownSent = ownChildEvents.find((event) => event.type === 'agent.thread_message_sent');
    assert.equal(ownSent.content[0].text, 'researched: 42');
    assert.equal(ownSent.to_session_thread_id, primary.id);
    assert.equal(ownSent.to_agent_name, undefined);
    assert.ok(!ownChildEvents.some((event) => event.type.startsWith('session.status_')));

    // Follow-up cause/effect graph: C1 the child is Idle with committed history;
    // C2 the coordinator addresses the prior receipt's session_thread_id; C3 the
    // child model can answer only if that Thread history is retained. E1 no new
    // Thread is created; E2 one deterministic fresh Run executes after the prior
    // Run; E3 the second report proves old assistant context is present; E4 each
    // Run/report remains exactly once. Decision rule F1=C1+C2+C3=>E1-E4.
    const followUpEvents = await runSession(client, ok.id, 'follow up with the same child');
    assert.equal(
      followUpEvents.filter((event) => event.type === 'session.thread_created').length,
      1,
      'F1/E1 follow-up reuses the existing child Thread',
    );
    assert.deepEqual(
      toolNames(followUpEvents),
      ['list_agents', 'send_to_agent', 'list_agents', 'send_to_agent'],
      'F1/E2 each coordinator Run uses the one fixed tool path',
    );
    assert.equal(
      followUpEvents.filter((event) => event.type === 'agent.thread_message_received').length,
      2,
      'F1/E4 each child Run reports exactly once',
    );
    assert.ok(
      receivedText(followUpEvents, 'researcher').includes('follow-up retained researched: 42'),
      'F1/E3 the fresh Run sees the same Thread history',
    );
    const followUpThreads = await listThreads(client, ok.id);
    assert.equal(followUpThreads.length, 2, 'F1/E1 primary + original child only');
    const followUpChildEvents = [];
    for await (const event of client.beta.sessions.threads.events.list(child.id, {
      session_id: ok.id,
      betas: BETAS,
    })) followUpChildEvents.push(event);
    assert.deepEqual(
      followUpChildEvents.map((event) => event.type),
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
      'F1/E2-E4 two ordered Runs and their model span pairs share one child Thread without agent.message',
    );
    const followUpUsage = (await client.beta.sessions.retrieve(ok.id, { betas: BETAS })).usage ?? {};
    assert.equal(followUpUsage.output_tokens, FAKE_USAGE.output_tokens * INFERENCES * 2,
      'F1/E4 the follow-up coordinator/child/report Runs are each charged once');

    // The optional selector is part of `user.interrupt`, not a parallel Thread
    // endpoint. It addresses only this child Run and the receipt projects onto
    // the selected child's stream (an omitted selector fans out; covered by the
    // protocol cause-table test with a recording runtime).
    const interruptReceipt = await client.beta.sessions.events.send(ok.id, {
      events: [{ type: 'user.interrupt', session_thread_id: child.id }],
      betas: BETAS,
    });
    assert.equal(interruptReceipt.data[0].type, 'user.interrupt');
    const afterInterrupt = [];
    for await (const event of client.beta.sessions.threads.events.list(child.id, {
      session_id: ok.id,
      betas: BETAS,
    })) afterInterrupt.push(event);
    assert.equal(afterInterrupt.at(-1).type, 'user.interrupt');
    assert.equal(afterInterrupt.at(-1).session_thread_id, child.id);
    await waitForSessionEventReceipt(
      client,
      ok.id,
      interruptReceipt.data[0].id,
      BETAS,
      () => true,
      'M5 targeted interrupt receipt to process after child projection',
      { timeoutMs: 30_000 },
    );

    // Archiving the child thread terminates it (session.thread_status_terminated).
    const archived = await client.beta.sessions.threads.archive(child.id, {
      session_id: ok.id,
      betas: BETAS,
    });
    assert.ok(archived.archived_at, 'the child thread carries archived_at');
    const afterEvents = await listEvents(client, ok.id);
    const terminated = afterEvents.find(
      (e) => e.type === 'session.thread_status_terminated' && e.session_thread_id === child.id,
    );
    assert.ok(terminated, 'archiving the child emits session.thread_status_terminated');
    assert.equal(terminated.agent_name, 'researcher');
    const terminalCount = afterEvents.filter(
      (event) => event.type === 'session.thread_status_terminated' && event.session_thread_id === child.id,
    ).length;
    const archivedAgain = await client.beta.sessions.threads.archive(child.id, {
      session_id: ok.id,
      betas: BETAS,
    });
    assert.equal(archivedAgain.status, 'terminated');
    assert.equal(
      (await listEvents(client, ok.id)).filter(
        (event) => event.type === 'session.thread_status_terminated' && event.session_thread_id === child.id,
      ).length,
      terminalCount,
      'repeated archive is behaviorally idempotent and emits no duplicate terminal event',
    );
    const childStream = await client.beta.sessions.threads.events.stream(child.id, {
      session_id: ok.id,
      betas: BETAS,
    });
    const streamedChildTypes = [];
    for await (const event of childStream) streamedChildTypes.push(event.type);
    assert.ok(streamedChildTypes.includes('session.thread_status_terminated'));
    assert.ok(!streamedChildTypes.some((type) => type.startsWith('session.status_')));
    assert.equal(
      streamedChildTypes.filter((type) => type === 'agent.message').length,
      0,
      'a report-only child stream never duplicates terminal reports as agent.message',
    );

    // M7-M9 cause/effect detail: C1 the same ordinary child Run owns a two-call
    // ToolBatch but only its exact ResumeTicket classifies the first occurrence;
    // C2 the sibling is initially withheld; C3 control selects the Run directly,
    // by global fan-out, or through archive; C4 cancellation classifies both.
    // Effects: E1 the first ask is visible exactly once before control; E2 the
    // sibling and both fixed ordered results appear exactly once after C4; E3
    // requires_action -> end_turn performs no inference or primary report Run;
    // E4 archive adds one terminated transition. K: stable source-qualified ids
    // plus the committed Event log own catch-up/deduplication; the fake upstream
    // request ledger remains the sole sampling authority.
    {
      const waiting = await createAwaitingChild(client, 'interrupt the awaiting child directly');
      const requestsBeforeControl = upstream.requests.length;
      const receipt = await client.beta.sessions.events.send(waiting.session.id, {
        events: [{ type: 'user.interrupt', session_thread_id: waiting.childId }],
        betas: BETAS,
      });
      assert.equal(receipt.data[0].type, 'user.interrupt', 'M7 targeted receipt');
      const settled = await waitForSettledChild(client, waiting.session.id, waiting.childId);
      await waitForSessionEventReceipt(
        client,
        waiting.session.id,
        receipt.data[0].id,
        BETAS,
        () => true,
        'M7 targeted interrupt receipt to process before settlement acceptance',
        { timeoutMs: 30_000 },
      );
      const childReceipt = [...settled.childEvents]
        .reverse()
        .find((event) => event.type === 'user.interrupt');
      assert.equal(childReceipt?.session_thread_id, waiting.childId, 'M7 selected child projection');
      assertInterruptedBatch(
        waiting,
        settled,
        requestsBeforeControl,
        upstream.requests.length,
        'M7 targeted interrupt',
      );
    }

    {
      const waiting = await createAwaitingChild(client, 'globally interrupt the awaiting child');
      const requestsBeforeControl = upstream.requests.length;
      const receipt = await client.beta.sessions.events.send(waiting.session.id, {
        events: [{ type: 'user.interrupt' }],
        betas: BETAS,
      });
      assert.equal(receipt.data[0].type, 'user.interrupt', 'M8 global receipt');
      const settled = await waitForSettledChild(client, waiting.session.id, waiting.childId);
      await waitForSessionEventReceipt(
        client,
        waiting.session.id,
        receipt.data[0].id,
        BETAS,
        () => true,
        'M8 global interrupt receipt to process before settlement acceptance',
        { timeoutMs: 30_000 },
      );
      assertInterruptedBatch(
        waiting,
        settled,
        requestsBeforeControl,
        upstream.requests.length,
        'M8 global interrupt',
      );
    }

    {
      const waiting = await createAwaitingChild(client, 'archive the awaiting child');
      const requestsBeforeControl = upstream.requests.length;
      const archivedAwaiting = await client.beta.sessions.threads.archive(waiting.childId, {
        session_id: waiting.session.id,
        betas: BETAS,
      });
      assert.equal(archivedAwaiting.status, 'terminated', 'M9 one archive request settles internally');
      const settled = await waitForSettledChild(client, waiting.session.id, waiting.childId);
      assertInterruptedBatch(
        waiting,
        settled,
        requestsBeforeControl,
        upstream.requests.length,
        'M9 archive',
      );
      assert.equal(
        settled.childEvents.filter((event) => event.type === 'session.thread_status_terminated').length,
        1,
        'M9/E4 one terminal transition',
      );
    }

    // M10-M11 cause/effect graph: C1 one Native child Run has an ordered two-call
    // ToolBatch; C2 only the first exact ResumeTicket initially classifies an
    // occurrence and the unclassified sibling is withheld; C3 a request contains
    // a wrong or duplicate reply; C4 the first exact permission decision is
    // allow; C5 the remaining committed ResumeTicket classifies the sibling and
    // receives deny. Effects: E1 C3 rejects before receipt/effect/sample; E2
    // C2+C4 resumes the same Run, appends only the missing sibling, preserves the
    // first ask/text once, and exposes exact C5; E3 C2+C5 denies that call; E4
    // the child and root each sample once after the complete batch and committed
    // usage counts each once. K: ResumeTicket/ToolResult are classification
    // authority; stable occurrence ids and the Event log prevent replay doubles.
    //
    // | Rule | exact owner/id | batch identities | decision | Effects |
    // |---|---|---|---|---|
    // | M10a | yes | exact + wrong | allow | E1 HTTP 400, atomic no-op |
    // | M10b | yes | exact + duplicate | allow | E1 HTTP 400, atomic no-op |
    // | M11a | yes | one current | confirmation allow | E2, next permission ticket, no sample |
    // | M11b | yes | one current | confirmation deny | E3,E4, one terminal child/report |
    {
      const requestStart = upstream.requests.length;
      const waiting = await createAwaitingChild(client, 'confirm the awaiting child exactly');
      const requestsAtAwait = upstream.requests.length;
      assert.equal(requestsAtAwait - requestStart, 4, 'M11 root admission + child await sample once each');
      assert.ok(
        waiting.toolUses.some((toolUse) => toolUse.id === waiting.pendingEventId),
        'M11 initial requires_action names the qualified child tool Event id',
      );
      const exactAllow = {
        type: 'user.tool_confirmation',
        tool_use_id: waiting.pendingEventId,
        result: 'allow',
      };
      const childIdsBeforeReject = waiting.childEvents.map((event) => event.id);
      const primaryIdsBeforeReject = waiting.primaryEvents.map((event) => event.id);

      const wrongStatus = await statusOf(
        client.beta.sessions.events.send(waiting.session.id, {
          events: [
            exactAllow,
            {
              type: 'user.tool_confirmation',
              tool_use_id: 'evt_managed_tool_wrong',
              result: 'allow',
            },
          ],
          betas: BETAS,
        }),
        'M10a exact plus wrong confirmation batch',
      );
      assert.equal(wrongStatus, 400, 'M10a rejects the whole batch as bad_request');

      const duplicateStatus = await statusOf(
        client.beta.sessions.events.send(waiting.session.id, {
          events: [exactAllow, exactAllow],
          betas: BETAS,
        }),
        'M10b duplicate/ambiguous confirmation batch',
      );
      assert.equal(duplicateStatus, 400, 'M10b one pending identity cannot be consumed twice');
      assert.deepEqual(
        (await listThreadEvents(client, waiting.session.id, waiting.childId)).map((event) => event.id),
        childIdsBeforeReject,
        'M10/E1 rejected batches append no child receipt or result',
      );
      assert.deepEqual(
        (await listEvents(client, waiting.session.id)).map((event) => event.id),
        primaryIdsBeforeReject,
        'M10/E1 rejected batches append no primary receipt or lifecycle',
      );
      assert.equal(upstream.requests.length, requestsAtAwait, 'M10/E1 rejected batches do not sample');

      const allowReceipt = (await client.beta.sessions.events.send(waiting.session.id, {
        events: [exactAllow],
        betas: BETAS,
      })).data[0];
      const next = await waitForNextChildAction(
        client,
        waiting.session.id,
        waiting.childId,
        waiting.idle.id,
      );
      // M11a receipt rule: C5 exact allow receipt; E5 it is processed before the
      // next occurrence-qualified child action is accepted. K1 the child action
      // remains a Thread-state oracle, while canonical history owns completion.
      // Decision M11a+C5=>E2+E5.
      await waitForSessionEventReceipt(
        client,
        waiting.session.id,
        allowReceipt.id,
        BETAS,
        () => true,
        'M11a exact allow receipt to process',
        { timeoutMs: 30_000 },
      );
      assert.notEqual(next.pendingEventId, waiting.pendingEventId, 'M11a next call has a distinct Event id');
      const nextToolUses = next.childEvents.filter((event) => event.type === 'agent.tool_use');
      assert.equal(nextToolUses.length, 2, 'M11a later ticket appends only the withheld sibling');
      assert.equal(
        nextToolUses.filter((toolUse) => toolUse.id === waiting.pendingEventId).length,
        1,
        'M11a first ticket-qualified ask remains exactly once',
      );
      assert.ok(
        nextToolUses.some((toolUse) => toolUse.id === next.pendingEventId),
        'M11a next boundary selects the other call from the same committed ToolBatch',
      );
      for (const event of waiting.childEvents.filter(
        (candidate) => candidate.type === 'agent.message' || candidate.type === 'agent.thinking',
      )) {
        assert.equal(
          next.childEvents.filter((candidate) => candidate.id === event.id).length,
          1,
          'M11a partial catch-up does not duplicate already-projected child text',
        );
      }
      assert.deepEqual(
        next.childEvents
          .filter((event) => event.type.startsWith('session.thread_status_'))
          .map((event) => [event.type, event.stop_reason?.type ?? null]),
        [
          ['session.thread_status_running', null],
          ['session.thread_status_idle', 'requires_action'],
          ['session.thread_status_running', null],
          ['session.thread_status_idle', 'requires_action'],
        ],
        'M11a partial resolution resumes, then pauses with only the remaining blocker',
      );
      assert.deepEqual(
        next.idle.stop_reason.event_ids,
        [next.pendingEventId],
        'M11a the second requires_action boundary exposes only the unresolved call',
      );
      assert.equal(upstream.requests.length, requestsAtAwait, 'M11a partial ToolBatch resume performs no model sample');

      const denyReceipt = (await client.beta.sessions.events.send(waiting.session.id, {
        events: [{
          type: 'user.tool_confirmation',
          tool_use_id: next.pendingEventId,
          result: 'deny',
          deny_message: 'operator denied the second child call',
        }],
        betas: BETAS,
      })).data[0];
      const settled = await waitForSettledChild(
        client,
        waiting.session.id,
        waiting.childId,
        'confirmed child and root report settle',
      );
      // M11b receipt rule: C6 exact deny receipt; E6 it is processed before the
      // observed child/root terminal boundary. K2 prior allow state is excluded.
      // Decision M11b+C6=>E3+E4+E6.
      await waitForSessionEventReceipt(
        client,
        waiting.session.id,
        denyReceipt.id,
        BETAS,
        () => true,
        'M11b exact deny receipt to process',
        { timeoutMs: 30_000 },
      );
      const results = settled.childEvents.filter((event) => event.type === 'agent.tool_result');
      assert.deepEqual(
        results.map((result) => result.tool_use_id),
        [waiting.pendingEventId, next.pendingEventId],
        'M11/E2-E3 results preserve the one ToolBatch order and public identities',
      );
      assert.equal(results[0].is_error, false, 'M11/E2 allowed write executed');
      assert.equal(results[1].is_error, true, 'M11/E3 denied bash did not execute');
      assert.match(
        JSON.stringify(results[1].content),
        /blocked: operator denied the second child call/u,
        'M11/E3 denied call commits the model-visible blocked result',
      );
      assert.ok(
        !JSON.stringify(results[1].content).includes('must not execute'),
        'M11/E3 denied bash exposes no command output',
      );
      assert.deepEqual(
        settled.childEvents
          .filter((event) => event.type.startsWith('session.thread_status_'))
          .map((event) => [event.type, event.stop_reason?.type ?? null]),
        [
          ['session.thread_status_running', null],
          ['session.thread_status_idle', 'requires_action'],
          ['session.thread_status_running', null],
          ['session.thread_status_idle', 'requires_action'],
          ['session.thread_status_running', null],
          ['session.thread_status_idle', 'end_turn'],
        ],
        'M11/E2-E3 each exact reply resumes the same paused Run before its next boundary',
      );
      assert.equal(
        settled.primaryEvents.filter(
          (event) => event.type === 'agent.message'
            && event.content?.[0]?.text === 'coordination completed from child report',
        ).length,
        1,
        'M11/E4 root performs exactly one report sample',
      );
      assert.equal(
        upstream.requests.length - requestsAtAwait,
        2,
        'M11/E4 exactly one child continuation and one root report sample follow control',
      );
      const confirmedThread = await client.beta.sessions.threads.retrieve(waiting.childId, {
        session_id: waiting.session.id,
        betas: BETAS,
      });
      assert.equal(
        confirmedThread.usage?.output_tokens,
        FAKE_USAGE.output_tokens * 2,
        'M11/E4 child initial and terminal model samples are each charged once',
      );
      const confirmedUsage = (await client.beta.sessions.retrieve(waiting.session.id, { betas: BETAS })).usage ?? {};
      assert.equal(
        confirmedUsage.output_tokens,
        FAKE_USAGE.output_tokens * 6,
        'M11/E4 Session charges three root admission, two child, and one root report samples once',
      );
    }

    // M12-M14 cause/effect graph: C1 the primary exposes the compiler-owned
    // reserved Advisor descriptor and exact candidate; C2 its child Run ends
    // naturally, fails at the Provider, or is cancelled while Running; C3 the
    // projector observes copied root context and possibly in-flight child text;
    // C4 M14 retains the exact root User and targeted-interrupt receipts.
    // Effects: E1 one isolated `{type:advisor,model}` Thread owns the ordinary
    // Run and its exactly paired model start/end spans; E2 only natural
    // completion emits one advice receive and makes it available to the root
    // continuation; E3 failure/cancel expose no partial child content or
    // Provider detail; E4 every terminal Advisor self-terminates; E5 usage is
    // partitioned as two root samples plus one child sample and the Session
    // aggregate equals their sum; E6 after M14 settles, both C4 receipts are
    // processed without treating either admission response as a child result.
    //
    // | Rule | child boundary | committed advice | Effect |
    // |---|---|---|---|
    // | M12 | natural end | complete | E1,E2,E4,E5 |
    // | M13 | failed | none/partial | E1,E3,E4 |
    // | M14 | cancelled | none/partial | E1,E3,E4,E6 |
    {
      const requestStart = upstream.requests.length;
      const advisorSession = await client.beta.sessions.create({
        agent: 'assistant',
        environment_id: 'env_local',
        betas: BETAS,
      });
      await runSession(client, advisorSession.id, 'consult the advisor successfully');
      const evidence = await settledAdvisorEvidence(client, advisorSession.id);
      assert.equal(evidence.threads.length, 2, 'M12/E1 root plus one isolated Advisor Thread');
      assert.deepEqual(
        evidence.advisor.agent,
        { type: 'advisor', model: 'claude-opus-4-8' },
        'M12/E1 Advisor uses the closed two-field Thread identity',
      );
      assert.deepEqual(
        evidence.childEvents.map((event) => event.type),
        [
          'session.thread_status_running',
          'span.model_request_start',
          'span.model_request_end',
          'session.thread_status_idle',
          'session.thread_status_terminated',
        ],
        'M12/E1-E4 Advisor delivery is root-only while its isolated Thread owns the model and terminal lifecycle',
      );
      const advisorModelStart = evidence.childEvents.find(
        (event) => event.type === 'span.model_request_start',
      );
      const advisorModelEnd = evidence.childEvents.find(
        (event) => event.type === 'span.model_request_end',
      );
      assert.equal(
        advisorModelEnd.model_request_start_id,
        advisorModelStart.id,
        'M12/E1 the ordinary Advisor child Run owns one exactly paired model span',
      );
      assert.match(
        receivedText(evidence.events, 'anthropic.advisor'),
        /independent advisor advice/u,
        'M12/E2 complete advice crosses from the Advisor Thread to root',
      );
      assert.ok(
        messages(evidence.events).some((message) => message.includes('root used advisor advice')),
        'M12/E2 root continuation observes and uses the returned advice',
      );
      assert.deepEqual(toolNames(evidence.events), [], 'M12 reserved Advisor is not an ordinary public tool event');
      const advisorRoot = evidence.threads.find((thread) => thread.parent_thread_id === null);
      assert.equal(
        advisorRoot.usage?.output_tokens,
        FAKE_USAGE.output_tokens * 2,
        'M12/E5 root Thread charges its initial call and advice continuation once each',
      );
      assert.equal(
        evidence.advisor.usage?.output_tokens,
        FAKE_USAGE.output_tokens,
        'M12/E5 Advisor child sample is charged once',
      );
      assert.equal(
        evidence.session.usage?.output_tokens,
        FAKE_USAGE.output_tokens * 3,
        `M12/E5 Session folds root plus Advisor usage once; requests=${upstream.requests.length - requestStart}, root=${advisorRoot.usage?.output_tokens}, child=${evidence.advisor.usage?.output_tokens}, session=${evidence.session.usage?.output_tokens}`,
      );
      assert.equal(upstream.requests.length - requestStart, 3, 'M12 exactly three Provider requests');
    }

    // M2: `{type:"self"}` is compiled as an explicit recursive edge, not an ordinary
    // owner-id cycle. The child executes the same frozen Native Agent snapshot and
    // remains context-isolated in its own Thread.
    const selfSession = await client.beta.sessions.create({
      agent: 'assistant',
      environment_id: 'env_local',
      betas: BETAS,
    });
    const selfEvents = await runSession(client, selfSession.id, 'use the self agent');
    assert.deepEqual(toolNames(selfEvents), ['list_agents', 'send_to_agent']);
    assert.ok(messages(selfEvents).some((message) => message.includes('coordination accepted:')));
    assert.ok(!messages(selfEvents).some((message) => message.includes('self copy: 42')));
    const selfCreated = selfEvents.find(
      (event) => event.type === 'session.thread_created' && event.agent_name === 'assistant',
    );
    assert.ok(selfCreated, 'the self copy owns an isolated child Thread');
    const selfChild = await client.beta.sessions.threads.retrieve(selfCreated.session_thread_id, {
      session_id: selfSession.id,
      betas: BETAS,
    });
    const selfPrimary = (await listThreads(client, selfSession.id))
      .find((thread) => thread.parent_thread_id === null);
    assert.match(selfPrimary.id, /^sthr_/u, 'M2 primary uses one public Thread ID codec');
    assert.equal(selfChild.agent.id, 'assistant');
    assert.equal(selfChild.parent_thread_id, selfPrimary.id);
    assert.match(receivedText(selfEvents, 'assistant'), /self copy: 42/);

    // M3: the same parent-mediated lifecycle routes an ACP roster member by its
    // frozen backend_ref; its committed reply is another cross-Thread event.
    const acpSession = await client.beta.sessions.create({
      agent: 'assistant',
      environment_id: 'env_local',
      betas: BETAS,
    });
    const acpEvents = await runSession(client, acpSession.id, 'use the acp agent');
    assert.deepEqual(toolNames(acpEvents), ['list_agents', 'send_to_agent']);
    assert.ok(messages(acpEvents).some((message) => message.includes('coordination accepted:')));
    assert.ok(!messages(acpEvents).some((message) => message.includes('acp-runtime reply')));
    const acpCreated = acpEvents.find(
      (event) => event.type === 'session.thread_created' && event.agent_name === 'acp-worker',
    );
    assert.ok(acpCreated, 'the ACP Agent owns an ordinary child Thread');
    const acpChild = await client.beta.sessions.threads.retrieve(acpCreated.session_thread_id, {
      session_id: acpSession.id,
      betas: BETAS,
    });
    const acpPrimary = (await listThreads(client, acpSession.id))
      .find((thread) => thread.parent_thread_id === null);
    assert.match(acpPrimary.id, /^sthr_/u, 'M3 primary uses one public Thread ID codec');
    assert.equal(acpChild.agent.id, 'acp-worker');
    assert.equal(acpChild.parent_thread_id, acpPrimary.id);
    assert.match(receivedText(acpEvents, 'acp-worker'), /acp-runtime reply/);

    // M4: `ghost` is absent from the frozen roster, so send rejects before any
    // child Thread or child inference exists.
    const bad = await client.beta.sessions.create({ agent: 'assistant', environment_id: 'env_local', betas: BETAS });
    const badEvents = await runSession(client, bad.id, 'use the ghost agent');
    const badText = badEvents
      .filter((e) => e.type === 'agent.message' || e.type === 'agent.tool_result')
      .map((e) => e.content?.[0]?.text ?? '')
      .join(' | ');
    assert.ok(
      /roster|published targets/i.test(badText),
      `roster rejection surfaced: ${badText}`,
    );
    assert.ok(!badText.includes('researched: 42'), `no sub-run output leaked: ${badText}`);
    assert.deepEqual(toolNames(badEvents), ['list_agents', 'send_to_agent']);
    assert.ok(!badEvents.some((event) => event.type === 'session.thread_created'));

    console.log('E2E PASS: fixed Managed multi-Agent coordination via TS SDK.');
  }, { SESSION_DEPLOYMENT_SANDBOX_TIER: 'local' });

  await withScenarioServer(
    'delegate',
    'delegating',
    PORT + 1,
    async (baseUrl, upstream) => {
      const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: baseUrl });
      const session = await client.beta.sessions.create({
        agent: 'assistant',
        environment_id: 'env_local',
        betas: BETAS,
      });
      await runSession(client, session.id, 'consult the advisor with a provider failure');
      const evidence = await settledAdvisorEvidence(client, session.id);
      const serialized = JSON.stringify({ events: evidence.events, child: evidence.childEvents });
      assert.ok(
        upstream.requests.some((request) => request.model === 'fake-advisor' && request.failed === true),
        'M13 Provider failure occurs on the exact frozen Advisor candidate',
      );
      assert.ok(
        !evidence.childEvents.some((event) => event.type === 'agent.thread_message_received'),
        'M13/E3 failed Advisor emits no partial advice receive',
      );
      assert.ok(
        !evidence.events.some((event) => event.type === 'agent.thread_message_received'),
        'M13/E3 failed Advisor emits no partial advice receive on root',
      );
      assert.ok(!serialized.includes('independent advisor advice'), 'M13/E3 no advice leaks');
      assert.ok(!serialized.includes('model fake-advisor is down'), 'M13/E3 Provider detail is private');
      assert.ok(
        messages(evidence.events).includes('root handled the generic advisor failure'),
        'M13 root receives only the stable generic failure',
      );
    },
    { SESSION_DEPLOYMENT_SANDBOX_TIER: 'local' },
    { upstream: { failModel: 'fake-advisor' } },
  );

  await withScenarioServer(
    'delegate',
    'delegating',
    PORT + 2,
    async (baseUrl) => {
      const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: baseUrl });
      const session = await client.beta.sessions.create({
        agent: 'assistant',
        environment_id: 'env_local',
        betas: BETAS,
      });
      const driving = client.beta.sessions.events.send(session.id, {
        events: [{
          type: 'user.message',
          content: [{ type: 'text', text: 'consult the advisor then cancel it' }],
        }],
        betas: BETAS,
      });
      const running = await waitForValue(
        () => listThreads(client, session.id),
        (threads) => threads.some(
          (thread) => thread.agent?.type === 'advisor' && thread.status === 'running',
        ),
        'Advisor Thread did not reach Running before cancellation',
        { timeoutMs: 30_000 },
      ).then((threads) => threads.find(
        (thread) => thread.agent?.type === 'advisor' && thread.status === 'running',
      ));
      const receipt = await client.beta.sessions.events.send(session.id, {
        events: [{ type: 'user.interrupt', session_thread_id: running.id }],
        betas: BETAS,
      });
      assert.equal(receipt.data[0].session_thread_id, running.id, 'M14 exact child selector receipt');
      const drivingReceipt = (await driving).data[0];
      assert.equal(drivingReceipt?.type, 'user.message', 'M14 exact driving User receipt');
      const evidence = await settledAdvisorEvidence(client, session.id);
      await waitForSessionEventReceipt(
        client,
        session.id,
        receipt.data[0].id,
        BETAS,
        () => true,
        'M14 targeted Advisor cancel receipt to process before terminal evidence',
        { timeoutMs: 30_000 },
      );
      await waitForSessionEventReceipt(
        client,
        session.id,
        drivingReceipt.id,
        BETAS,
        () => true,
        'M14 driving User receipt to process after targeted cancellation settles',
        { timeoutMs: 30_000 },
      );
      const serialized = JSON.stringify({ events: evidence.events, child: evidence.childEvents });
      assert.equal(evidence.advisor.id, running.id, 'M14/E1 cancellation retains the exact Advisor Thread');
      assert.ok(
        !evidence.childEvents.some((event) => event.type === 'agent.thread_message_received'),
        'M14/E3 cancelled Advisor emits no partial advice receive',
      );
      assert.ok(
        !evidence.events.some((event) => event.type === 'agent.thread_message_received'),
        'M14/E3 cancelled Advisor emits no partial advice receive on root',
      );
      assert.ok(!serialized.includes('independent advisor advice'), 'M14/E3 no in-flight advice leaks');
    },
    { SESSION_DEPLOYMENT_SANDBOX_TIER: 'local' },
    { upstream: { delayMs: 4_000 } },
  );
}

main().catch((err) => {
  console.error('E2E FAIL:', err);
  process.exit(1);
});
