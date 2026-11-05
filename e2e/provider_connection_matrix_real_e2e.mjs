// Real provider/dialect certification lane.
//
// Built-in credentials are discovered from their conventional environment
// variables. Arbitrary third-party providers are supplied through the
// secret-name-only AWAKEN_PROVIDER_CASES_JSON contract; secret bytes are read
// from the named environment variable and never enter the emitted artifact.
// AWAKEN_PROVIDER_MATRIX_MANAGED_MULTIAGENT=1 extends each selected case through
// the same owner with a real coordinator -> child -> report certification.

import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { pathToFileURL } from 'node:url';
import Anthropic from '@anthropic-ai/sdk';
import {
  cleanupFixtureTree,
  deploymentEnv,
  pass,
  publishManagementAgent,
  runCleanupSteps,
  provisionProviderTestModel,
  spawnServer,
  stopServer,
  waitForPort,
  waitForSessionEventReceipt,
} from './harness.mjs';
import {
  loadProviderCases,
  publicProviderCase,
  requiredProviderCaseIds,
  selectProviderCases,
} from './provider_compat_cases.mjs';
import { emitPublicReport } from './llm_eval_metrics.mjs';
import { assertThreadModelRequests, readSpans } from './trace_validate.mjs';

const BETAS = ['managed-agents-2026-04-01'];
const BASE_PORT = Number(process.env.E2E_PORT ?? 38327);

// SDK-decoded, task-owned public observations only. Tool arguments retain only
// this case's agent_id/message; no provider object, headers, env or error body.
// Text bytes remain exact. Non-text payloads are explicitly not captured here.
export function publicAttemptEvent(event) {
  if (!event || typeof event !== 'object') return { available: false };
  const fields = ['id', 'type', 'processed_at', 'session_thread_id',
    'from_session_thread_id', 'to_session_thread_id', 'tool_use_id', 'name'];
  const observed = Object.fromEntries(fields.filter(key => event[key] !== undefined)
    .map(key => [key, event[key]]));
  if (Array.isArray(event.content)) {
    observed.content = event.content.map(block => block?.type === 'text'
      ? { type: block.type, text: block.text }
      : { type: block?.type ?? null, payload_captured: false });
  } else if (event.content !== undefined) observed.content_captured = false;
  if (event.type === 'agent.tool_use') {
    observed.input = Object.fromEntries(['agent_id', 'message']
      .filter(key => typeof event.input?.[key] === 'string')
      .map(key => [key, event.input[key]]));
  }
  if (event.stop_reason !== undefined) {
    observed.stop_reason = { type: event.stop_reason?.type,
      ...(event.stop_reason?.event_ids ? { event_ids: event.stop_reason.event_ids } : {}) };
  }
  if (event.error !== undefined) observed.error = { type: event.error?.type };
  return observed;
}

async function managedRun(base, agent, marker, observation) {
  const client = new Anthropic({ apiKey: 'local-provider-certification', baseURL: base }); // awaken-allow(secret): (local fixture)
  const session = await client.beta.sessions.create({
    agent,
    environment_id: 'env_local',
    betas: BETAS,
  });
  observation.session_id = session.id;
  observation.boundary = 'send';
  // Single-Run provider cause/effect rule S1: C1=the official SDK returns one
  // durable User Event receipt; C2=provider execution may settle after that
  // HTTP response. E1=the exact receipt becomes processed; E2=only a later
  // Agent Message containing the certification marker and a later Session idle
  // prove completion. S1(C1+C2)->E1+E2; an immediate list is not a terminal
  // oracle and must not misclassify normal durable admission as provider failure.
  const receipt = await client.beta.sessions.events.send(session.id, {
    events: [{
      type: 'user.message',
      content: [{ type: 'text', text: `Reply with exactly ${marker}` }],
    }],
    betas: BETAS,
  });
  observation.accepted_receipt = receipt.data.map(publicAttemptEvent);
  const acceptedId = receipt.data[0]?.id;
  assert.equal(typeof acceptedId, 'string', 'S1 exact accepted User Event id');
  const { delta: events } = await waitForSessionEventReceipt(
    client,
    session.id,
    acceptedId,
    BETAS,
    ({ delta }) => {
      observation.events_after_receipt = delta.map(publicAttemptEvent);
      observation.boundary = 'receipt_and_terminal';
      return delta.some(
        (event) => event.type === 'agent.message' && eventText(event).includes(marker),
      ) && delta.some((event) => event.type === 'session.status_idle');
    },
    `${agent} exact provider certification Run to settle`,
    { timeoutMs: 120_000, pollMs: 200 },
  );
  assert.match(JSON.stringify(events), new RegExp(marker, 'u'));
  assert.ok(events.some((event) => event.type === 'session.status_idle'));
  observation.boundary = 'delete';
  await client.beta.sessions.delete(session.id, { betas: BETAS });
  observation.session_delete = { status: 'completed' };
  observation.status = 'passed';
  // DELETE acknowledges the lifecycle transition before asynchronous resource
  // cleanup drains. Keep the owned server alive long enough to observe that
  // boundary instead of cancelling cleanup during fixture teardown.
  await new Promise((resolve) => setTimeout(resolve, 500));
}

const eventText = (event) => (event.content ?? [])
  .filter((block) => block.type === 'text')
  .map((block) => block.text ?? '')
  .join('')
  .trim();

export function providerRunSelection(id, env = process.env) {
  const control = env.AWAKEN_PROVIDER_MATRIX_CHILD_LITERAL_CONTROL;
  assert.ok(control === undefined || control === '0' || control === '1',
    'child literal control must be explicitly 0 or 1');
  const childLiteralControl = control === '1';
  const multiagent = env.AWAKEN_PROVIDER_MATRIX_MANAGED_MULTIAGENT === '1';
  assert.ok(!(childLiteralControl && multiagent),
    'diagnostic child literal control cannot replace required multiagent certification');
  const prefix = `AWAKEN-MANAGED-${id.toUpperCase().replace(/[^A-Z0-9]+/gu, '-')}`;
  const markers = { childReply: `${prefix}-CHILD-OK`, sendAccepted: `${prefix}-SEND-ACCEPTED`,
    reportAck: `${prefix}-REPORT-ACK` };
  return { childLiteralControl, multiagent, markers,
    singleMarker: childLiteralControl ? markers.childReply : `AWAKEN-PROVIDER-${id.toUpperCase()}-OK` };
}

function frozenWorkerDeclaration(thread, worker, authoredSystem) {
  const agent = thread?.agent;
  if (!agent || agent.type !== 'agent' || agent.id !== worker) {
    return { available: false, reason: 'selected_worker_snapshot_unavailable' };
  }
  if (typeof authoredSystem !== 'string') {
    return { available: false, reason: 'authored_system_unavailable' };
  }
  const systemAvailable = Object.hasOwn(agent, 'system')
    && (typeof agent.system === 'string' || agent.system === null);
  const emptyPolicy = field => {
    const available = Object.hasOwn(agent, field) && Array.isArray(agent[field]);
    return { available, empty: available ? agent[field].length === 0 : null };
  };
  return {
    available: true,
    scope: 'frozen public Agent declaration; not actual request or runtime tool surface',
    system: { available: systemAvailable,
      matches_authored: systemAvailable ? agent.system === authoredSystem : null },
    tools: emptyPolicy('tools'),
    mcp_servers: emptyPolicy('mcp_servers'),
    skills: emptyPolicy('skills'),
  };
}

export async function exerciseManagedMultiagentAttempt(base, coordinator, worker, markers, observation, authoredModelId, publications, authoredWorkerSystem) {
  const client = new Anthropic({ apiKey: 'local-provider-certification', baseURL: base }); // awaken-allow(secret): (local fixture)
  const session = await client.beta.sessions.create({
    agent: coordinator,
    environment_id: 'env_local',
    betas: BETAS,
  });
  observation.session_id = session.id;
  observation.boundary = 'send';
  observation.raw_model_output = { available: false, reason: 'SDK events and structured spans are not raw provider output' };
  let failure;
  let childId;
  let threadsReadAttempted = false;
  const readThreads = async (afterFailure = false) => {
    threadsReadAttempted = true;
    try {
      const threads = [];
      for await (const thread of client.beta.sessions.threads.list(session.id, { betas: BETAS },
        afterFailure ? { maxRetries: 0 } : undefined)) threads.push(thread);
      observation.frozen_threads = { observed_at: new Date().toISOString(), after_failure: afterFailure,
        items: threads.map(thread => ({ id: thread.id, parent_thread_id: thread.parent_thread_id,
          status: thread.status, agent: { id: thread.agent?.id, version: thread.agent?.version,
            model_id: thread.agent?.model?.id } })),
        worker_authored_declaration: frozenWorkerDeclaration(
          threads.find(thread => thread.id === childId), worker, authoredWorkerSystem,
        ) };
      return threads;
    } catch (error) {
      observation.frozen_threads = { observed_at: new Date().toISOString(), after_failure: afterFailure,
        available: false, error: { name: error.name, code: error.code ?? null },
        worker_authored_declaration: { available: false, reason: 'threads_read_unavailable' } };
      throw error;
    }
  };
  try {
    // Multi-Agent receipt rule M1: C7 exact coordinator command receipt plus
    // C1-C6 protocol effects; E3 the receipt is processed only when aggregate
    // idle is durably listed. K1 older Session idle cannot satisfy this attempt.
    // Decision M1=C1-C7=>E1-E3.
    const receipt = (await client.beta.sessions.events.send(session.id, {
      events: [{
        type: 'user.message',
        content: [{ type: 'text', text: 'Start the managed coordination protocol now. Do not answer directly.' }],
      }],
      betas: BETAS,
    })).data[0];
    observation.accepted_receipt = publicAttemptEvent(receipt);
    let aggregate;
    const { events } = await waitForSessionEventReceipt(
      client,
      session.id,
      receipt.id,
      BETAS,
      async ({ delta }) => {
        observation.events_after_receipt = delta.map(publicAttemptEvent);
        observation.boundary = 'receipt_and_aggregate';
        aggregate = await client.beta.sessions.retrieve(session.id, { betas: BETAS });
        observation.aggregate = { id: aggregate.id, status: aggregate.status };
        if (aggregate.status === 'terminated') {
          throw new Error('real multi-Agent Session terminated before report acknowledgement');
        }
        return aggregate.status === 'idle'
          && delta.some((event) => event.type === 'session.status_idle');
      },
      'real managed multi-Agent aggregate report and idle',
      { timeoutMs: 60_000, pollMs: 200 },
    );
    assert.equal(aggregate?.status, 'idle', 'real multi-Agent aggregate reaches idle');
    assert.equal(events.at(-1)?.type, 'session.status_idle', 'aggregate idle is the terminal committed event');
    assert.equal(events.at(-1)?.stop_reason?.type, 'end_turn');

    const tools = events
      .filter((event) => event.type === 'agent.tool_use')
      .map((event) => event.name);
    assert.deepEqual(tools, ['list_agents', 'send_message'], 'one fixed list -> send sequence');

    const coordinatorMessages = events
      .filter((event) => event.type === 'agent.message')
      .map(eventText);
    assert.deepEqual(
      coordinatorMessages,
      [markers.sendAccepted, markers.reportAck],
      'the first Run acknowledges admission and the later report Run terminates without extra text',
    );

    const created = events.filter((event) => event.type === 'session.thread_created');
    assert.equal(created.length, 1, 'exactly one ordinary child Thread is created');
    childId = created[0].session_thread_id;
    observation.child_thread_id = childId;
    const childLifecycle = events.filter((event) => (
      event.session_thread_id === childId
      || event.to_session_thread_id === childId
      || event.from_session_thread_id === childId
    ));
    assert.deepEqual(
      childLifecycle.map((event) => event.type),
      [
        'session.thread_created',
        'session.thread_status_running',
        'agent.thread_message_sent',
        'agent.thread_message_received',
        'session.thread_status_idle',
      ],
      'the real child executes asynchronously on one ordinary Thread',
    );
    const legalThreadMessageTypes = new Set(['text', 'image', 'document', 'redacted']);
    assert.ok(
      childLifecycle[3].content.every((block) => legalThreadMessageTypes.has(block.type)),
      `child delivery uses only the official SDK content union: ${childLifecycle[3].content.map((block) => block.type)}`,
    );
    observation.boundary = 'exact_child_reply';
    observation.expected_child_reply = markers.childReply;
    assert.equal(eventText(childLifecycle[3]), markers.childReply, 'exact real child reply');
    assert.equal(childLifecycle[4].stop_reason?.type, 'end_turn');

    observation.boundary = 'frozen_model_assertions';
    const threads = await readThreads();
    assert.equal(threads.length, 2, 'primary + one real child Thread');
    const primary = threads.find((thread) => thread.parent_thread_id === null);
    const child = threads.find((thread) => thread.id === childId);
    assert.ok(primary && child, 'both primary and real child are addressable');
    assert.equal(child.parent_thread_id, primary.id);
    assert.equal(primary.agent.id, coordinator, 'the primary freezes the authored coordinator publication');
    assert.equal(child.agent.id, worker, 'the child freezes the authored worker publication');
    assert.equal(primary.agent.model.id, authoredModelId, 'the coordinator freezes the authored profile selection');
    assert.equal(child.agent.model.id, authoredModelId, 'the child freezes the authored profile selection');
    assert.equal(primary.agent.version, publications.coordinator.source_revision, 'the coordinator freezes its publication revision');
    assert.equal(child.agent.version, publications.worker.source_revision, 'the child freezes its publication revision');
    assert.equal(child.status, 'idle');
    observation.status = 'passed';
  } catch (error) {
    failure = error;
    observation.status = 'failed';
    observation.failure = { boundary: observation.boundary, name: error.name, code: error.code ?? null };
    throw error;
  } finally {
    // One post-failure read, before deletion, supplements missing facts only.
    // It is NOT a pre-failure snapshot and cannot replace the primary error.
    if (!threadsReadAttempted) {
      try { await readThreads(Boolean(failure)); }
      catch { /* readThreads retained its own unavailable facts; preserve the primary failure. */ }
    }
    if (childId) {
      try {
        const events = [];
        for await (const event of client.beta.sessions.threads.events.list(childId,
          { session_id: session.id, betas: BETAS }, { maxRetries: 0 })) events.push(publicAttemptEvent(event));
        observation.child_events = { observed_at: new Date().toISOString(), after_failure: Boolean(failure), events };
      } catch (error) { observation.child_events = { available: false, after_failure: Boolean(failure),
        error: { name: error.name, code: error.code ?? null } }; }
    } else observation.child_events = { available: false, reason: 'the case never selected an exact child Thread' };
    observation.session_delete = { status: 'attempted' };
    try {
      await client.beta.sessions.delete(session.id, { betas: BETAS });
      observation.session_delete.status = 'completed';
    } catch (cleanupError) {
      observation.session_delete = { status: 'failed', error: { name: cleanupError.name, code: cleanupError.code ?? null } };
      if (!failure) throw cleanupError;
    }
    await new Promise((resolve) => setTimeout(resolve, 500));
  }
}

export async function managedMultiagentRun(base, coordinator, worker, markers, observations, authoredModelId, publications, authoredWorkerSystem) {
  // Real-model cause/effect graph:
  // C1 the coordinator sees both fixed tools; C2 it follows list -> send once;
  // C3 the child produces an exact reply on its ordinary Thread; C4 the report
  // wakes a later coordinator Run; C5 child delivery uses only the SDK's public
  // Text/Image/Document/Redacted union; C6 both frozen Thread snapshots match
  // their authored profile, Agent identity and publication revision.
  // Effects are E1 two exact tool events, E2 one
  // child lifecycle, E3 one receipt acknowledgement, E4 one report
  // acknowledgement, E5 aggregate idle, E6 exact public child content, and E7
  // frozen selection identity. Actual request targets are separately audited
  // for both roles from flushed spans after strict shutdown; missing or wrong
  // targets reject the case even when this protocol succeeds. A format/selection
  // miss may retry the entire fresh Session, but no assertion is relaxed.
  //
  // | Rule | C1 | C2 | C3 | C4 | C5 | C6 | Effect |
  // | M1 | yes | yes | yes | yes | yes | yes | E1 + E2 + E3 + E4 + E5 + E6 + E7 |
  // | M2 | yes | no/repeated | any | any | any | any | reject attempt; no golden result |
  // | M3 | yes | yes | missing/wrong | any | any | any | reject attempt; no golden result |
  // | M4 | yes | yes | yes | missing/redelegates | any | any | reject attempt; no golden result |
  // | M5 | yes | yes | yes | yes | illegal block | any | reject attempt; no golden result |
  // | M6 | yes | yes | yes | yes | yes | wrong | reject attempt; no golden result |
  const maxAttempts = 3;
  const failures = [];
  for (let attempt = 1; attempt <= maxAttempts; attempt += 1) {
    const observation = { attempt, status: 'running', boundary: 'create' };
    observations.push(observation);
    try {
      await exerciseManagedMultiagentAttempt(base, coordinator, worker, markers, observation, authoredModelId, publications, authoredWorkerSystem);
      return;
    } catch (error) {
      observation.status = 'failed';
      observation.failure ??= { boundary: observation.boundary, name: error.name, code: error.code ?? null };
      const retryable = error?.code === 'ERR_ASSERTION'
        || String(error?.message ?? error).includes('did not settle');
      failures.push(`attempt ${attempt}: ${error?.message ?? error}`);
      if (!retryable || attempt === maxAttempts) break;
    }
  }
  throw new Error(`real Managed multi-Agent certification failed: ${failures.join(' | ')}`);
}

async function certifyProvider(item, index, result) {
  // L1 default -> original single/multiagent path. L2 explicit standalone
  // control -> the same Worker/publication/profile receives the exact child
  // command through managedRun, once. L3 combining modes refuses before any
  // fixture/Provider I/O. This observes literal copying without changing the
  // original coordinator prompt, three attempts, model or Structured policy.
  const selection = providerRunSelection(item.id, process.env);
  const directory = fs.mkdtempSync(path.join(os.tmpdir(), `awaken-provider-${item.id}-`));
  const port = BASE_PORT + index;
  const env = deploymentEnv(directory, { identityMode: 'no-login' });
  const traceFile = path.join(directory, 'provider.spans.jsonl');
  const certifyManagedMultiagent = selection.multiagent;
  let server;
  let failure;
  let started;
  result.fixture_directory = directory;
  result.boundary = 'fixture_build';
  result.selected_model = null;
  result.discovered_models = null;
  result.cleanup = { server: { status: 'not_started' }, directory: { status: 'retained' } };
  try {
    ({ server } = spawnServer('management-providers', port, { ...env, AWAKEN_TRACE_FILE: traceFile }));
    result.process = { pid: server.pid, port };
    started = performance.now();
    result.boundary = 'wait_for_port';
    await waitForPort(port, 180_000, server);
    const base = `http://127.0.0.1:${port}`;
    const workspace = fs.readFileSync(path.join(directory, 'platform-workspace-id'), 'utf8').trim();
    // This fixture's original profile id is URL-unreserved by the case loader.
    // The independent expected wire is test data, not a new model-id formatter.
    const profileId = `provider-${item.id}`;
    const authoredModelId = `profile=${profileId}`;
    result.boundary = 'discovery_and_exact_credential';
    const { offering, discoveredModels, model } = await provisionProviderTestModel(
      base, workspace, item, profileId,
    );

    result.authored_model = model;
    result.expected_authored_model_id = authoredModelId;
    result.selected_model = offering.model_id;
    result.discovered_models = discoveredModels;
    result.boundary = 'publication';
    const agent = `provider-cert-${item.id}`;
    const workerSystem = 'Follow the user instruction exactly and answer briefly.';
    const published = await publishManagementAgent(base, agent, {
      id: agent,
      name: `Provider certification ${item.id}`,
      system: workerSystem,
      max_steps: 2,
      model,
      tools: [],
    });
    assert.equal(published.installed, true, `${item.id}: agent was not installed`);
    result.publications = { worker: published };

    const marker = selection.singleMarker;
    result.boundary = 'single_managed_run';
    result.single_run = { status: 'running', boundary: 'create' };
    if (selection.childLiteralControl) {
      result.diagnostic_control = { scope: 'standalone child-literal control only',
        expected_child_reply: selection.markers.childReply, multiagent_certified: false };
    }
    await managedRun(base, agent, marker, result.single_run);
    if (selection.childLiteralControl) {
      // L4 contains-marker success is insufficient for this control: retain
      // the exact same public-text oracle as the original child assertion.
      // Extra/wrong/multiple replies fail and strict teardown still runs.
      const replies = result.single_run.events_after_receipt
        .filter(event => event.type === 'agent.message').map(eventText);
      assert.equal(replies.length, 1, 'exact standalone child-literal reply count');
      assert.equal(replies[0], selection.markers.childReply, 'exact standalone child-literal reply');
    }
    pass(`${item.id}: discovery -> exact credential -> publication -> real Managed Run`);
    if (certifyManagedMultiagent) {
      result.boundary = 'multiagent_publication';
      const coordinator = `${agent}-coordinator`;
      const markers = selection.markers;
      const publishedCoordinator = await publishManagementAgent(base, coordinator, {
        id: coordinator,
        name: `Provider certification ${item.id} coordinator`,
        system: [
          'You are a deterministic Managed multi-Agent coordinator under certification.',
          'Follow this protocol exactly and never call more than one tool in a response.',
          `If the latest user message begins with "Message from agent ", call no tools and reply exactly ${markers.reportAck}.`,
          'Otherwise first call list_agents exactly once and emit no text.',
          `After list_agents returns, call send_message exactly once with agent_id "${agent}" and message "Reply with exactly ${markers.childReply}". Emit no text.`,
          `After send_message returns, call no more tools and reply exactly ${markers.sendAccepted}.`,
          'Never answer the original request directly and never start another child after a child report.',
        ].join('\n'),
        max_steps: 8,
        model,
        tools: [],
        multiagent: {
          type: 'coordinator',
          agents: [{ type: 'agent', id: agent, version: 1 }],
        },
      });
      assert.equal(publishedCoordinator.installed, true, `${item.id}: coordinator was not installed`);
      result.publications.coordinator = publishedCoordinator;
      result.boundary = 'managed_multiagent';
      result.managed_attempts = [];
      await managedMultiagentRun(base, coordinator, agent, markers, result.managed_attempts, authoredModelId, result.publications, workerSystem);
      pass(`${item.id}: real coordinator -> fixed tools -> child -> report -> aggregate idle`);
    }
    Object.assign(result, { latency_ms: performance.now() - started,
      ...(certifyManagedMultiagent ? { managed_multiagent: 'passed' } : {}), protocol_status: 'passed', functional_status: 'passed' });
  } catch (error) {
    failure = error;
    result.protocol_status = 'failed';
    result.functional_status = 'failed';
    result.failure = { boundary: result.boundary, name: error.name, code: error.code ?? null };
    if (result.single_run?.status === 'running') {
      result.single_run.status = 'failed';
      result.single_run.failure = { boundary: result.single_run.boundary, name: error.name, code: error.code ?? null };
    }
  }
  try {
    await runCleanupSteps([async () => {
      let cleanupBoundary = 'server';
      try {
        if (server) {
          await stopServer(server, { requireCleanExit: true });
          result.cleanup.server = { status: 'passed', pid: server.pid,
            exit_code: server.exitCode, signal: server.signalCode };
          try {
            const attributes = ['gen_ai.operation.name', 'gen_ai.request.model', 'gen_ai.tool.name',
              'gen_ai.tool.call.id', 'awaken.run.id', 'awaken.thread.id', 'gen_ai.conversation.id',
              'http.route', 'http.response.status_code', 'otel.status_code'];
            result.trace = { scope: 'structured spans after strict shutdown; not raw model output',
              spans: readSpans(traceFile).map(span => ({ name: span.name, trace_id: span.trace_id,
                span_id: span.span_id, parent_span_id: span.parent_span_id,
                attributes: Object.fromEntries(attributes.filter(key => span.attributes?.[key] !== undefined)
                  .map(key => [key, span.attributes[key]])) })) };
          } catch (error) { result.trace = { available: false,
            error: { name: error.name, code: error.code ?? null } }; }
        }
        cleanupBoundary = 'directory';
        cleanupFixtureTree(directory);
        result.cleanup.directory.status = 'removed';
      } catch (error) {
        result.cleanup.error = { boundary: cleanupBoundary, name: error.name, code: error.code ?? null };
        if (server && cleanupBoundary === 'server') result.cleanup.server = { status: 'failed', pid: server.pid,
          exit_code: server.exitCode, signal: server.signalCode };
        throw error;
      }
    }, async () => {
      if (!certifyManagedMultiagent) {
        result.attribution = { status: 'not_requested' };
        return;
      }
      if (!result.managed_attempts) {
        result.attribution = { status: 'not_reached', reason: 'requested mode did not reach an attempt' };
        return;
      }
      if (result.cleanup.server.status !== 'passed') {
        result.attribution = { status: 'unproven', reason: 'strict server stop did not prove a flushed trace' };
        return;
      }
      try {
        assert.ok(Array.isArray(result.managed_attempts) && result.managed_attempts.length > 0,
          'requested multiagent certification requires observed attempts');
        const attempts = result.managed_attempts.map(attempt => ({ attempt: attempt.attempt,
          ...assertThreadModelRequests(result.trace?.spans, result.selected_model,
            { coordinator: attempt.session_id, child: attempt.child_thread_id }) }));
        result.attribution = { status: 'passed', attempts };
      } catch (error) {
        result.functional_status = 'failed';
        result.attribution = { status: 'failed', error: { name: error.name, code: error.code ?? null } };
        throw error;
      }
    }], failure ? [failure] : []);
    result.status = 'passed';
  } catch (error) {
    result.status = 'failed';
    throw error;
  }
  return result;
}

async function main() {
  const report = { version: 1, results: [] };
  if (process.env.AWAKEN_PROVIDER_MATRIX_CHILD_LITERAL_CONTROL === '1') {
    report.scope = 'diagnostic standalone child-literal control';
  }
  let failure;
  try {
    // Test design (real-provider matrix). Causes: C1=zero or more canonical
    // provider cases have live credentials; C2=strict required ids select an
    // ordered subset; C3=Managed multi-Agent certification is enabled. Effects:
    // E1=no cases skips unless strict; E2=each selected provider completes
    // discovery, exact credential binding, publication, and one marked Managed
    // Run; E3=C3 additionally completes coordinator->child->report once.
    // Constraints/invariant: provider cases come only from the canonical catalog,
    // secrets never enter evidence, and each result names its exact dialect/model.
    // Decision rules: P0=!C1&&!strict=>E1; P1=C1+C2=>E2;
    // P2=P1+C3=>E2+E3; missing required identity fails before provider I/O.
    const cases = loadProviderCases();
    const requiredCaseIds = requiredProviderCaseIds();
    if (cases.length === 0) {
      if (process.env.AWAKEN_PROVIDER_MATRIX_REQUIRE_CASES === '1' || requiredCaseIds.length > 0) {
        throw new Error('provider certification requires at least one configured provider case');
      }
      console.log('SKIP provider_connection_matrix_real_e2e: no provider credentials configured.');
      return;
    }
    const selectedCases = selectProviderCases(cases, requiredCaseIds);
    const results = report.results;
    for (const [index, item] of selectedCases.entries()) {
      const result = { ...publicProviderCase(item), status: 'running' };
      results.push(result);
      await certifyProvider(item, index, result);
    }
  } catch (error) {
    failure = error;
    report.failure = { name: error.name, code: error.code ?? null };
    throw error;
  }
  finally {
    report.status = failure ? 'failed' : report.results.length ? 'passed' : 'skipped';
    try { emitPublicReport('AWAKEN_PROVIDER_COMPAT', 'provider-compat', report); }
    catch (error) {
      if (!failure) throw error;
      console.error('Provider report write failed:', error.name, error.code ?? 'unknown');
    }
  }
  if (report.scope) console.log(`DIAGNOSTIC: ${report.results.length} standalone child-literal control(s).`);
  else console.log(`E2E PASS: ${report.results.length} real provider/dialect compatibility case(s).`);
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  main().catch((error) => {
    console.error('E2E FAIL:', error);
    process.exitCode = 1;
  });
}
