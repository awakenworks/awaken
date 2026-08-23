// Official @anthropic-ai/sdk Managed Session compatibility matrix for Native
// plus every canonical ACP catalog runtime. This is deliberately orthogonal to
// the SDK-version handoff suite: it does not repeat archive/delete or old/current
// client permutations. One catalog-derived set of cases drives the production
// route registry and the one official ACP JSON-RPC scenario fixture.
//
// Offline evidence boundary: this suite proves catalog selection, projection,
// common codec/session behavior, and Managed protocol semantics without keys.
// Executable discovery/version, login/credential realization, provider model
// behavior, MCP-server startup, and actual mounted Memory file I/O remain owned
// by `ACP_RUNTIMES=all ACP_MEMORY_REQUIRE_RUNTIMES=1` in
// acp_runtime_memory_matrix_e2e.mjs; a deterministic subprocess cannot prove
// those runtime-specific external facts.

import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import Anthropic from '@anthropic-ai/sdk';
import {
  cleanupFixtureTree,
  pass,
  realServerEnv,
  spawnServer,
  startUpstream,
  stopServer,
  waitForPort,
  waitForSessionEventReceipt,
  waitForValue,
  withServer,
} from './harness.mjs';
import { ACP_RUNTIME_IDS } from './acp_runtime_profiles.mjs';

const BETAS = ['managed-agents-2026-04-01'];
const MEMORY_HEADERS = { 'anthropic-beta': 'agent-memory-2026-07-22' };
const PORT = Number(process.env.E2E_PORT);
const ROOT = path.join(os.tmpdir(), `awaken-managed-native-acp-matrix-${process.pid}`);
const INTERRUPT_READY_DIR = path.join(ROOT, 'interrupt-ready');
const SERVER_ENV = {
  SESSION_DEPLOYMENT_STORAGE_DIR: path.join(ROOT, 'store'),
  SESSION_DEPLOYMENT_SANDBOX_DIR: path.join(ROOT, 'sandboxes'),
  AWAKEN_MATRIX_INTERRUPT_READY_DIR: INTERRUPT_READY_DIR,
};
const CASES = Object.freeze([
  Object.freeze({ runtime: 'native', agent: 'matrix-native', kind: 'native' }),
  ...ACP_RUNTIME_IDS.map((runtime) => Object.freeze({
    runtime,
    agent: `matrix-acp-${runtime}`,
    kind: 'acp',
  })),
]);

assert.deepEqual(
  CASES.map(({ runtime }) => runtime),
  ['native', ...ACP_RUNTIME_IDS],
  'the offline matrix has one Native column plus every canonical ACP catalog column',
);

const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

async function drain(items) {
  const values = [];
  for await (const item of items) values.push(item);
  return values;
}

const listEvents = (client, sessionId) => drain(
  client.beta.sessions.events.list(sessionId, { betas: BETAS }),
);

function eventText(events, type = 'agent.message') {
  return events
    .filter((event) => event.type === type)
    .flatMap((event) => event.content ?? [])
    .map((content) => content.text ?? '')
    .join(' ');
}

function expectedReply(cell, phase, opened = 'new') {
  return cell.kind === 'native'
    ? `Echo: matrix-basic ${phase} ${cell.runtime}`
    : `matrix-${cell.runtime}-reply open=${opened}`;
}

function usageUnits(usage = {}) {
  return (usage.input_tokens ?? 0)
    + (usage.output_tokens ?? 0)
    + (usage.cache_read_input_tokens ?? 0)
    + (usage.cache_creation?.ephemeral_5m_input_tokens ?? 0);
}

async function sendBatch(client, sessionId, events, terminalEffect, description) {
  const receipt = await client.beta.sessions.events.send(sessionId, { events, betas: BETAS });
  const receiptIds = receipt.data.map((event) => event.id);
  assert.ok(
    receiptIds.length === events.length && receiptIds.every((id) => typeof id === 'string'),
    `${description}: the official SDK returns one exact receipt per input`,
  );
  // Batch receipt rule B1: C1=one exact id per ordered input and C2=the last
  // id commits; E1=all batch ids must be committed in request order before the
  // caller's effect can pass. Constraint: the last receipt scopes delta and the
  // caller still owns the terminal oracle. Decision: !C1|!C2=>retry/fail;
  // C1+C2+caller effect=>return the full list, exact delta, and last Session.
  let lastSession;
  const settled = await waitForSessionEventReceipt(
    client,
    sessionId,
    receiptIds.at(-1),
    BETAS,
    async ({ events: listed, delta }) => {
      lastSession = await client.beta.sessions.retrieve(sessionId, { betas: BETAS });
      const indexes = receiptIds.map((id) => listed.findIndex(
        (event) => event.id === id && event.processed_at,
      ));
      if (indexes.some((index) => index < 0)) return false;
      assert.deepEqual(
        indexes,
        [...indexes].sort((left, right) => left - right),
        `${description}: batch receipts retain request order`,
      );
      return terminalEffect({ session: lastSession, listed, delta });
    },
    description,
    { timeoutMs: 30_000, pollMs: 25 },
  );
  return {
    session: lastSession,
    listed: settled.events,
    delta: settled.delta,
  };
}

async function waitForInitial(client, sessionId, marker, initialText) {
  return waitForValue(
    async () => {
      const [session, events] = await Promise.all([
        client.beta.sessions.retrieve(sessionId, { betas: BETAS }),
        listEvents(client, sessionId),
      ]);
      return { session, events };
    },
    ({ session, events }) => session.status === 'idle'
      && eventText(events).includes(marker)
      && events.some((event) => event.type === 'user.message'
        && event.processed_at
        && event.content?.some((content) => content.text === initialText))
      && events.some((event) => event.type === 'session.status_running')
      && [...events].reverse().some(
        (event) => event.type === 'session.status_idle'
          && event.stop_reason?.type === 'end_turn',
      ),
    `initial Run to project ${marker}`,
    { timeoutMs: 30_000, pollMs: 25 },
  );
}

async function exerciseDirectMatrix() {
  cleanupFixtureTree(ROOT);
  fs.mkdirSync(ROOT, { recursive: true });
  fs.mkdirSync(INTERRUPT_READY_DIR, { recursive: true });
  let running = spawnServer('acp-jsonrpc', PORT, SERVER_ENV);
  const sessions = new Map();
  try {
    await waitForPort(PORT, 900_000, running.server);
    let client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: running.baseUrl });
    const memory = await client.post('/v1/memory_stores', {
      body: { name: 'native-acp-offline-resource-binding' },
      headers: MEMORY_HEADERS,
    });

    // Test design D1, applied once to each of the six CASES rows.
    // Causes: C1=published backend identity; C2=create-time User input;
    // C3=one typed MemoryStore binding. Effects: E1=the exact runtime reply;
    // E2=running -> idle/end_turn projection; E3=processed input, nonzero usage,
    // and an unchanged resource manifest; E4=ACP completed tool call/result pair.
    // Constraint: Native has no ACP tool projection; all ACP rows use the same
    // fake codec and differ only through the production catalog route.
    // Decision rules: D1-N(C1=Native+C2+C3)->E1-E3;
    // D1-A(C1=each ACP id+C2+C3)->E1-E4.
    for (const cell of CASES) {
      const initialText = `matrix-basic initial ${cell.runtime}`;
      const created = await client.beta.sessions.create({
        agent: cell.agent,
        environment_id: 'env_local',
        resources: [{
          type: 'memory_store',
          memory_store_id: memory.id,
          mount_path: '/memory',
        }],
        initial_events: [{
          type: 'user.message',
          content: [{ type: 'text', text: initialText }],
        }],
        betas: BETAS,
      });
      const initial = await waitForInitial(
        client,
        created.id,
        expectedReply(cell, 'initial'),
        initialText,
      );
      const persistedUser = initial.events.find(
        (event) => event.type === 'user.message'
          && event.content?.some((content) => content.text === initialText),
      );
      assert.ok(persistedUser?.processed_at, `${cell.runtime}: initial User input committed`);
      assert.ok(usageUnits(initial.session.usage) > 0, `${cell.runtime}: projected usage is nonzero`);
      const resources = await drain(
        client.beta.sessions.resources.list(created.id, { betas: BETAS }),
      );
      assert.equal(resources.length, 1, `${cell.runtime}: one create-time resource binding`);
      assert.equal(resources[0].memory_store_id, memory.id, `${cell.runtime}: exact MemoryStore id`);
      if (cell.kind === 'acp') {
        const toolUse = initial.events.find(
          (event) => event.type === 'agent.tool_use' && event.name === 'read',
        );
        const toolResult = initial.events.find(
          (event) => event.type === 'agent.tool_result'
            && event.tool_use_id === toolUse?.id,
        );
        assert.ok(toolUse, `${cell.runtime}: ACP tool call projected`);
        assert.ok(toolResult, `${cell.runtime}: ACP tool reply retained exact identity`);
        assert.ok(
          eventText([toolResult], 'agent.tool_result').includes('matrix tool result'),
          `${cell.runtime}: ACP tool reply content projected`,
        );
      }
      sessions.set(cell.runtime, {
        id: created.id,
        usage: usageUnits(initial.session.usage),
      });
      pass(`${cell.runtime}: create/initial/status/usage/resource projection`);
    }

    await stopServer(running.server);
    running = null;
    running = spawnServer('acp-jsonrpc', PORT, SERVER_ENV);
    await waitForPort(PORT, 900_000, running.server);
    client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: running.baseUrl });

    // Test design D2, applied once to every retained Session.
    // Causes: C1=D1 committed history; C2=fresh server process over the same
    // storage; C3=User+final System batch. Effects: E1=Session/resource recovery;
    // E2=both exact receipts commit; E3=a fresh Run idles; E4=usage increases;
    // E5=ACP uses session/load, while Native resumes its ordinary history.
    // Constraint: this proves Awaken's durable ACP session-id handoff, not a real
    // vendor CLI's persistence implementation. Decision rule D2=C1+C2+C3=>E1-E5.
    for (const cell of CASES) {
      const prior = sessions.get(cell.runtime);
      const retrieved = await client.beta.sessions.retrieve(prior.id, { betas: BETAS });
      assert.equal(retrieved.id, prior.id, `${cell.runtime}: Session rehydrated`);
      assert.equal(retrieved.resources?.[0]?.memory_store_id, memory.id, `${cell.runtime}: resource rehydrated`);
      const userText = `matrix-basic recovered ${cell.runtime}`;
      const systemText = `matrix-system ${cell.runtime}`;
      const recovered = await sendBatch(
        client,
        prior.id,
        [
          { type: 'user.message', content: [{ type: 'text', text: userText }] },
          { type: 'system.message', content: [{ type: 'text', text: systemText }] },
        ],
        ({ session, delta }) => session.status === 'idle'
          && eventText(delta).includes(expectedReply(cell, 'recovered', 'load'))
          && usageUnits(session.usage) > prior.usage
          && delta.some((event) => event.type === 'session.status_idle'
            && event.stop_reason?.type === 'end_turn'),
        `${cell.runtime}: restart recovery and User+System continuation`,
      );
      const persistedSystem = recovered.listed.find(
        (event) => event.type === 'system.message'
          && event.content?.some((content) => content.text === systemText),
      );
      assert.ok(persistedSystem?.processed_at, `${cell.runtime}: exact System input committed`);
      assert.ok(
        usageUnits(recovered.session.usage) > prior.usage,
        `${cell.runtime}: usage accumulated across restart; prior=${prior.usage}, current=${usageUnits(recovered.session.usage)}`,
      );
      pass(`${cell.runtime}: restart/resource/User+System recovery`);
    }

    // Test design D3, applied to every ACP row.
    // Causes: C1=ACP permission request; C2=its qualified Managed tool-use id;
    // C3=official SDK allow. Effects: E1=requires_action names C2; E2=the ACP
    // option reply resumes the same logical turn; E3=allowed marker + end_turn.
    // Constraint: runtime-local `matrix-permission-call` is never used as the
    // client reply id. Decision rule D3=C1+C2+C3=>E1-E3.
    for (const runtime of ACP_RUNTIME_IDS) {
      const sessionId = sessions.get(runtime).id;
      const waiting = await sendBatch(
        client,
        sessionId,
        [{ type: 'user.message', content: [{ type: 'text', text: `matrix-hitl ${runtime}` }] }],
        ({ session, delta }) => {
          const tool = delta.find(
            (event) => event.type === 'agent.tool_use' && event.evaluated_permission === 'ask',
          );
          return session.status === 'idle' && tool !== undefined && delta.some(
            (event) => event.type === 'session.status_idle'
              && event.stop_reason?.type === 'requires_action'
              && event.stop_reason.event_ids.includes(tool.id),
          );
        },
        `${runtime}: ACP permission request to requires_action`,
      );
      const tool = waiting.delta.find(
        (event) => event.type === 'agent.tool_use' && event.evaluated_permission === 'ask',
      );
      assert.notEqual(tool.id, 'matrix-permission-call', `${runtime}: public tool id is qualified`);
      const allowed = await sendBatch(
        client,
        sessionId,
        [{ type: 'user.tool_confirmation', tool_use_id: tool.id, result: 'allow' }],
        ({ session, delta }) => session.status === 'idle'
          && eventText(delta).includes(`matrix-${runtime}-hitl-allowed`)
          && delta.some((event) => event.type === 'session.status_idle'
            && event.stop_reason?.type === 'end_turn'),
        `${runtime}: ACP permission allow to terminal reply`,
      );
      assert.ok(eventText(allowed.delta).includes(`matrix-${runtime}-hitl-allowed`));
      pass(`${runtime}: qualified ACP HITL allow/resume`);
    }

    // Test design D4, applied to every ACP row.
    // Causes: C1=malformed deterministic ACP frame; C2=later valid User turn.
    // Effects: E1=stable public failure (no adapter crash/detail authority);
    // E2=Session returns idle; E3=C2 succeeds on the same Session.
    // Constraint: real auth/rate-limit/login classifications belong to the live
    // runtime lane. Decision rules D4-F(C1)->E1+E2; D4-R(C1+C2)->E3.
    for (const runtime of ACP_RUNTIME_IDS) {
      const sessionId = sessions.get(runtime).id;
      const failed = await sendBatch(
        client,
        sessionId,
        [{ type: 'user.message', content: [{ type: 'text', text: `matrix-error ${runtime}` }] }],
        ({ session, delta }) => session.status === 'idle'
          && (/agent turn failed/iu.test(eventText(delta))
            || delta.some((event) => event.type === 'session.error')),
        `${runtime}: malformed ACP frame to stable failure`,
      );
      assert.ok(
        /agent turn failed/iu.test(eventText(failed.delta))
          || failed.delta.some((event) => event.type === 'session.error'),
        `${runtime}: failure is publicly observable`,
      );
      const recovered = await sendBatch(
        client,
        sessionId,
        [{ type: 'user.message', content: [{ type: 'text', text: `matrix-basic error-recovery ${runtime}` }] }],
        ({ session, delta }) => session.status === 'idle'
          && eventText(delta).includes(`matrix-${runtime}-reply`),
        `${runtime}: valid turn after ACP failure`,
      );
      assert.ok(eventText(recovered.delta).includes(`matrix-${runtime}-reply`));
      pass(`${runtime}: ACP failure classification and same-Session recovery`);
    }

    // Test design D5, applied to every ACP row.
    // Causes: C1=slow active ACP prompt with an exact User receipt; C2=official
    // user.interrupt; C3=later valid User turn. Effects: E1=C1 emits its exact
    // readiness witness and its receipt processes after interruption; E2=C2 kills
    // the owned subprocess and returns idle without its late marker; E3=C3 succeeds.
    // Constraint: readiness only synchronizes the control edge; receipt completion
    // and all behavioral effects are asserted from committed events, never timing. Decision rules:
    // D5-I(C1+C2)->E1+E2; D5-R(D5-I+C3)->E3.
    for (const runtime of ACP_RUNTIME_IDS) {
      const sessionId = sessions.get(runtime).id;
      const before = await listEvents(client, sessionId);
      const readyPath = path.join(INTERRUPT_READY_DIR, runtime);
      fs.rmSync(readyPath, { force: true });
      const active = client.beta.sessions.events.send(sessionId, {
        events: [{
          type: 'user.message',
          content: [{ type: 'text', text: `matrix-interrupt ${runtime}` }],
        }],
        betas: BETAS,
      });
      await waitForValue(
        () => fs.existsSync(readyPath),
        (ready) => ready,
        `${runtime}: slow ACP prompt to reach its blocking branch`,
        { timeoutMs: 10_000, pollMs: 10 },
      );
      const interrupted = await sendBatch(
        client,
        sessionId,
        [{ type: 'user.interrupt' }],
        ({ session, delta }) => session.status === 'idle'
          && delta.some((event) => event.type === 'session.status_idle'),
        `${runtime}: ACP interrupt to terminal idle`,
      );
      const activeReceipt = (await active).data[0];
      assert.equal(activeReceipt?.type, 'user.message', `${runtime}: exact interrupted User receipt`);
      await waitForSessionEventReceipt(
        client,
        sessionId,
        activeReceipt.id,
        BETAS,
        () => true,
        `${runtime}: original User receipt to process after ACP interrupt`,
      );
      const afterActive = interrupted.listed.slice(before.length);
      assert.ok(
        !eventText(afterActive).includes(`matrix-${runtime}-uncancelled`),
        `${runtime}: interrupted subprocess emitted no late reply`,
      );
      const steered = await sendBatch(
        client,
        sessionId,
        [{ type: 'user.message', content: [{ type: 'text', text: `matrix-basic steered ${runtime}` }] }],
        ({ session, delta }) => session.status === 'idle'
          && eventText(delta).includes(`matrix-${runtime}-reply`),
        `${runtime}: follow-up after ACP interrupt`,
      );
      assert.ok(eventText(steered.delta).includes(`matrix-${runtime}-reply`));
      pass(`${runtime}: ACP interrupt/no-late-output/recovery`);
    }
  } finally {
    if (running) await stopServer(running.server);
    cleanupFixtureTree(ROOT);
  }
}

async function exerciseNativeControlRows() {
  // Test design N1 (Native tool/HITL).
  // Causes: C1=Native mutating tool reaches ask; C2=official allow for its exact
  // id. Effects: E1=requires_action; E2=tool result/read-back and end_turn.
  // Constraint: no allow-all gate. Decision rule N1=C1+C2=>E1+E2.
  await withServer('probe', PORT + 1, async (baseURL) => {
    const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL });
    const session = await client.beta.sessions.create({
      agent: 'assistant', environment_id: 'env_local', betas: BETAS,
    });
    const waiting = await sendBatch(
      client,
      session.id,
      [{ type: 'user.message', content: [{ type: 'text', text: 'NATIVE-MATRIX-HITL' }] }],
      ({ delta }) => {
        const tool = delta.find(
          (event) => event.type === 'agent.tool_use' && event.evaluated_permission === 'ask',
        );
        return tool !== undefined && delta.some(
          (event) => event.type === 'session.status_idle'
            && event.stop_reason?.type === 'requires_action'
            && event.stop_reason.event_ids.includes(tool.id),
        );
      },
      'Native mutating tool to requires_action',
    );
    const tool = waiting.delta.find(
      (event) => event.type === 'agent.tool_use' && event.evaluated_permission === 'ask',
    );
    const allowed = await sendBatch(
      client,
      session.id,
      [{ type: 'user.tool_confirmation', tool_use_id: tool.id, result: 'allow' }],
      ({ session: current, delta }) => current.status === 'idle'
        && delta.some((event) => event.type === 'agent.tool_result')
        && delta.some((event) => event.type === 'session.status_idle'
          && event.stop_reason?.type === 'end_turn'),
      'Native allow to tool result and end_turn',
    );
    assert.ok(eventText(allowed.delta, 'agent.tool_result').includes('NATIVE-MATRIX-HITL'));
    pass('native: qualified HITL allow and tool reply');
  });

  // Test design N2 (Native error/recovery).
  // Causes: C1=permanent Native provider fault; C2=later valid User turn.
  // Effects: E1=session.error; E2=C2 replies on the same Session.
  // Constraint: the accepted POST is not treated as the terminal effect.
  // Decision rules N2-F(C1)->E1; N2-R(C1+C2)->E2.
  await withServer('error', PORT + 2, async (baseURL) => {
    const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL });
    const session = await client.beta.sessions.create({
      agent: 'assistant', environment_id: 'env_local', betas: BETAS,
    });
    const failed = await sendBatch(
      client,
      session.id,
      [{ type: 'user.message', content: [{ type: 'text', text: 'matrix BOOM' }] }],
      ({ delta }) => delta.some((event) => event.type === 'session.error'),
      'Native provider fault to session.error',
    );
    assert.ok(failed.delta.some((event) => event.type === 'session.error'));
    const recovered = await sendBatch(
      client,
      session.id,
      [{ type: 'user.message', content: [{ type: 'text', text: 'native recovered' }] }],
      ({ session: current, delta }) => current.status === 'idle'
        && eventText(delta).includes('Echo: native recovered'),
      'Native turn after session.error',
    );
    assert.ok(eventText(recovered.delta).includes('Echo: native recovered'));
    pass('native: session.error and same-Session recovery');
  });

  // Test design N3 (Native interrupt/recovery).
  // Causes: C1=slow Native provider Run with an exact User receipt;
  // C2=user.interrupt; C3=replacement User turn. Effects: E1=C1 reaches running
  // and its receipt processes after interruption; E2=C2 returns idle; E3=C3
  // completes. Constraint: the upstream is deterministic and local, and the
  // original receipt is fenced only after the interrupt terminal edge. Decision rule
  // N3=C1+C2+C3=>E1+E2+E3.
  const upstream = await startUpstream('echo', { delayMs: 900 });
  const server = spawnServer('real', PORT + 3, realServerEnv('echo', upstream));
  try {
    await waitForPort(PORT + 3, 900_000, server.server);
    const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: server.baseUrl });
    const session = await client.beta.sessions.create({
      agent: 'assistant', environment_id: 'env_local', betas: BETAS,
    });
    const active = client.beta.sessions.events.send(session.id, {
      events: [{ type: 'user.message', content: [{ type: 'text', text: 'native slow original' }] }],
      betas: BETAS,
    });
    await waitForValue(
      () => listEvents(client, session.id),
      (events) => events.some((event) => event.type === 'session.status_running'),
      'Native slow Run to reach running',
      { timeoutMs: 10_000, pollMs: 10 },
    );
    const interruptReceipt = (await client.beta.sessions.events.send(session.id, {
      events: [{ type: 'user.interrupt' }], betas: BETAS,
    })).data[0];
    const activeReceipt = (await active).data[0];
    assert.equal(activeReceipt?.type, 'user.message', 'Native exact interrupted User receipt');
    await waitForSessionEventReceipt(
      client,
      session.id,
      interruptReceipt.id,
      BETAS,
      ({ delta }) => delta.some((event) => event.type === 'session.status_idle'),
      'Native interrupt receipt to process into idle',
      { timeoutMs: 10_000, pollMs: 10 },
    );
    await waitForSessionEventReceipt(
      client,
      session.id,
      activeReceipt.id,
      BETAS,
      () => true,
      'Native original User receipt to process after interrupt idle',
      { timeoutMs: 10_000, pollMs: 10 },
    );
    const recovered = await sendBatch(
      client,
      session.id,
      [{ type: 'user.message', content: [{ type: 'text', text: 'native replacement' }] }],
      ({ session: current, delta }) => current.status === 'idle'
        && eventText(delta).includes('native replacement'),
      'Native replacement after interrupt',
    );
    assert.ok(eventText(recovered.delta).includes('native replacement'));
    pass('native: active interrupt and replacement turn');
  } finally {
    await stopServer(server.server);
    upstream.close();
  }
}

async function exerciseDelegationMatrix() {
  // Test design G1, one Native child plus every ACP catalog child.
  // Causes: C1=frozen coordinator roster; C2=runtime-specific target prompt.
  // Effects: E1=one child Thread with the selected Agent; E2=one asynchronous
  // cross-Thread reply from that Agent; E3=root returns idle.
  // Constraint: ACP children share the canonical codec; ACP roots do not gain
  // Native-only coordination tools. Decision rule G1=C1+C2=>E1-E3.
  await withServer('delegate', PORT + 4, async (baseURL) => {
    const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL });
    const rows = [
      { runtime: 'native', target: 'researcher', prompt: 'research the answer', marker: 'researched: 42' },
      ...ACP_RUNTIME_IDS.map((runtime) => ({
        runtime,
        target: `acp-${runtime}-worker`,
        prompt: `use the ${runtime} acp agent`,
        marker: `matrix-${runtime}-reply`,
      })),
    ];
    for (const row of rows) {
      const session = await client.beta.sessions.create({
        agent: 'assistant', environment_id: 'env_local', betas: BETAS,
      });
      const completed = await sendBatch(
        client,
        session.id,
        [{ type: 'user.message', content: [{ type: 'text', text: row.prompt }] }],
        ({ session: current, listed }) => current.status === 'idle'
          && listed.some(
            (event) => event.type === 'session.thread_created'
              && event.agent_name === row.target,
          )
          && listed.some(
            (event) => event.type === 'agent.thread_message_received'
              && event.from_agent_name === row.target
              && eventText([event], 'agent.thread_message_received').includes(row.marker),
          ),
        `${row.runtime}: delegated child reply`,
      );
      const created = completed.listed.find(
        (event) => event.type === 'session.thread_created' && event.agent_name === row.target,
      );
      assert.ok(created, `${row.runtime}: selected child Thread exists`);
      assert.ok(
        completed.listed.some(
          (event) => event.type === 'agent.thread_message_received'
            && event.from_agent_name === row.target
            && eventText([event], 'agent.thread_message_received').includes(row.marker),
        ),
        `${row.runtime}: selected child reply is committed`,
      );
      pass(`${row.runtime}: delegated child uses common Managed lifecycle`);
    }
  });
}

async function main() {
  await exerciseDirectMatrix();
  await exerciseNativeControlRows();
  await exerciseDelegationMatrix();
  console.log(
    `E2E PASS: official SDK Managed matrix covers Native + ${ACP_RUNTIME_IDS.join('/')}.`,
  );
}

main().catch(async (error) => {
  console.error('E2E FAIL:', error);
  // An interrupted failing assertion may leave a sleeping fixture child briefly
  // alive; the harness owns process shutdown and this removes only this test's
  // exact storage/sandbox tree after those handles have settled.
  await sleep(25);
  process.exitCode = 1;
});
