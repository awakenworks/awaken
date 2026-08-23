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
import Anthropic from '@anthropic-ai/sdk';
import {
  deploymentEnv,
  pass,
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

const BETAS = ['managed-agents-2026-04-01'];
const BASE_PORT = Number(process.env.E2E_PORT ?? 38327);

async function request(base, method, uri, body) {
  const response = await fetch(`${base}${uri}`, {
    method,
    headers: body === undefined ? {} : { 'content-type': 'application/json' },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  const text = await response.text();
  return { status: response.status, body: text ? JSON.parse(text) : null };
}

async function managedRun(base, agent, marker) {
  const client = new Anthropic({ apiKey: 'local-provider-certification', baseURL: base }); // awaken-allow: secret (local fixture)
  const session = await client.beta.sessions.create({
    agent,
    environment_id: 'env_local',
    betas: BETAS,
  });
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
  const acceptedId = receipt.data[0]?.id;
  assert.equal(typeof acceptedId, 'string', 'S1 exact accepted User Event id');
  const { delta: events } = await waitForSessionEventReceipt(
    client,
    session.id,
    acceptedId,
    BETAS,
    ({ delta }) => delta.some(
      (event) => event.type === 'agent.message' && eventText(event).includes(marker),
    )
      && delta.some((event) => event.type === 'session.status_idle'),
    `${agent} exact provider certification Run to settle`,
    { timeoutMs: 120_000, pollMs: 200 },
  );
  assert.match(JSON.stringify(events), new RegExp(marker, 'u'));
  assert.ok(events.some((event) => event.type === 'session.status_idle'));
  await client.beta.sessions.delete(session.id, { betas: BETAS });
  // DELETE acknowledges the lifecycle transition before asynchronous resource
  // cleanup drains. Keep the owned server alive long enough to observe that
  // boundary instead of cancelling cleanup during fixture teardown.
  await new Promise((resolve) => setTimeout(resolve, 500));
}

async function publishConfigAgent(base, agent, config, label = agent) {
  let result = await request(base, 'PUT', `/v1/config/agents/${agent}`, config);
  assert.equal(result.status, 200, `${label}: author agent`);
  result = await request(base, 'POST', `/v1/config/agents/${agent}/publish`);
  assert.equal(result.status, 200, `${label}: publish agent`);
  assert.equal(result.body.installed, true, `${label}: agent was not installed`);
}

const eventText = (event) => (event.content ?? [])
  .filter((block) => block.type === 'text')
  .map((block) => block.text ?? '')
  .join('')
  .trim();

async function exerciseManagedMultiagentAttempt(base, coordinator, worker, servedModel, markers) {
  const client = new Anthropic({ apiKey: 'local-provider-certification', baseURL: base }); // awaken-allow: secret (local fixture)
  const session = await client.beta.sessions.create({
    agent: coordinator,
    environment_id: 'env_local',
    betas: BETAS,
  });
  let failure;
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
    let aggregate;
    const { events } = await waitForSessionEventReceipt(
      client,
      session.id,
      receipt.id,
      BETAS,
      async ({ delta }) => {
        aggregate = await client.beta.sessions.retrieve(session.id, { betas: BETAS });
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
    assert.deepEqual(tools, ['list_agents', 'send_to_agent'], 'one fixed list -> send sequence');

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
    const childId = created[0].session_thread_id;
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
    assert.equal(eventText(childLifecycle[3]), markers.childReply, 'exact real child reply');
    assert.equal(childLifecycle[4].stop_reason?.type, 'end_turn');

    const threads = [];
    for await (const thread of client.beta.sessions.threads.list(session.id, { betas: BETAS })) {
      threads.push(thread);
    }
    assert.equal(threads.length, 2, 'primary + one real child Thread');
    const primary = threads.find((thread) => thread.parent_thread_id === null);
    const child = threads.find((thread) => thread.id === childId);
    assert.ok(primary && child, 'both primary and real child are addressable');
    assert.equal(child.parent_thread_id, primary.id);
    assert.equal(child.agent.id, worker, 'the child freezes the authored worker publication');
    assert.equal(primary.agent.model.id, servedModel, 'the coordinator freezes the selected provider model');
    assert.equal(child.agent.model.id, servedModel, 'the child freezes the same selected provider model');
    assert.equal(child.status, 'idle');
  } catch (error) {
    failure = error;
    throw error;
  } finally {
    try {
      await client.beta.sessions.delete(session.id, { betas: BETAS });
    } catch (cleanupError) {
      if (!failure) throw cleanupError;
    }
    await new Promise((resolve) => setTimeout(resolve, 500));
  }
}

async function managedMultiagentRun(base, coordinator, worker, servedModel, markers) {
  // Real-model cause/effect graph:
  // C1 the coordinator sees both fixed tools; C2 it follows list -> send once;
  // C3 the child produces an exact reply on its ordinary Thread; C4 the report
  // wakes a later coordinator Run; C5 child delivery uses only the SDK's public
  // Text/Image/Document/Redacted union; C6 both frozen Thread snapshots name the
  // exact model selected from this provider profile. Effects are E1 two exact tool events, E2 one
  // child lifecycle, E3 one receipt acknowledgement, E4 one report
  // acknowledgement, E5 aggregate idle, E6 exact public child content, and E7
  // exact provider/model attribution. A provider-format/model-choice
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
    try {
      await exerciseManagedMultiagentAttempt(base, coordinator, worker, servedModel, markers);
      return;
    } catch (error) {
      const retryable = error?.code === 'ERR_ASSERTION'
        || String(error?.message ?? error).includes('did not settle');
      failures.push(`attempt ${attempt}: ${error?.message ?? error}`);
      if (!retryable || attempt === maxAttempts) break;
    }
  }
  throw new Error(`real Managed multi-Agent certification failed: ${failures.join(' | ')}`);
}

async function certifyProvider(item, index) {
  const directory = fs.mkdtempSync(path.join(os.tmpdir(), `awaken-provider-${item.id}-`));
  const port = BASE_PORT + index;
  const env = deploymentEnv(directory, { identityMode: 'no-login' });
  const { server } = spawnServer('management-providers', port, env);
  const started = performance.now();
  try {
    await waitForPort(port, 180_000, server);
    const base = `http://127.0.0.1:${port}`;
    const workspace = fs.readFileSync(path.join(directory, 'platform-workspace-id'), 'utf8').trim();
    const authentication = item.auth === 'oauth_helper'
      ? {
          configuration: item.configuration,
          oauth_helper: item.oauth_helper,
        }
      : { base_url: item.base_url, secret: item.secret };
    let result = await request(base, 'POST', '/v1/config/provider-connections', {
      idempotency_key: `provider-certification-${item.id}`,
      workspace_id: workspace,
      provider_id: item.provider_id,
      display_name: `Compatibility ${item.id}`,
      dialect: item.dialect,
      timeout_secs: item.timeout_secs,
      ...authentication,
    });
    assert.equal(result.status, 201, `${item.id}: provider connection failed (${result.status})`);
    assert.ok(result.body.sync.discovered > 0, `${item.id}: provider discovered no models`);
    if (item.secret !== undefined) {
      assert.ok(!JSON.stringify(result.body).includes(item.secret), `${item.id}: secret leaked`);
    }
    const discoveredModels = result.body.sync.discovered;

    const credentialId = result.body.credential.id;
    result = await request(base, 'GET', '/v1/config/catalog');
    assert.equal(result.status, 200, `${item.id}: catalog`);
    const offerings = result.body.offerings.filter((offering) => (
      offering.provider_id === item.provider_id && (offering.status ?? 'active') === 'active'
    ));
    const offering = offerings.find((candidate) => candidate.model_id === item.model_id)
      ?? offerings[0];
    assert.ok(offering, `${item.id}: no active offering`);

    result = await request(base, 'PUT', `/v1/config/inference-profiles/provider-${item.id}`, {
      workspace_id: workspace,
      primary: {
        target: {
          model_id: offering.model_id,
          provider_id: offering.provider_id,
          protocol_endpoint_id: offering.protocol_endpoint_id,
        },
        credential_binding: { type: 'exact', credential_source_id: credentialId },
      },
      fallbacks: [],
      disabled_endpoint_ids: [],
    });
    assert.equal(result.status, 200, `${item.id}: inference profile`);

    const agent = `provider-cert-${item.id}`;
    await publishConfigAgent(base, agent, {
      id: agent,
      name: `Provider certification ${item.id}`,
      system: 'Follow the user instruction exactly and answer briefly.',
      max_steps: 2,
      model: { mode: 'profile', profile_id: `provider-${item.id}` },
      tools: [],
    }, item.id);

    const marker = `AWAKEN-PROVIDER-${item.id.toUpperCase()}-OK`;
    await managedRun(base, agent, marker);
    pass(`${item.id}: discovery -> exact credential -> publication -> real Managed Run`);
    const certifyManagedMultiagent = process.env.AWAKEN_PROVIDER_MATRIX_MANAGED_MULTIAGENT === '1';
    if (certifyManagedMultiagent) {
      const coordinator = `${agent}-coordinator`;
      const markerPrefix = `AWAKEN-MANAGED-${item.id.toUpperCase().replace(/[^A-Z0-9]+/gu, '-')}`;
      const markers = {
        childReply: `${markerPrefix}-CHILD-OK`,
        sendAccepted: `${markerPrefix}-SEND-ACCEPTED`,
        reportAck: `${markerPrefix}-REPORT-ACK`,
      };
      await publishConfigAgent(base, coordinator, {
        id: coordinator,
        name: `Provider certification ${item.id} coordinator`,
        system: [
          'You are a deterministic Managed multi-Agent coordinator under certification.',
          'Follow this protocol exactly and never call more than one tool in a response.',
          `If the latest user message begins with "Message from agent ", call no tools and reply exactly ${markers.reportAck}.`,
          'Otherwise first call list_agents exactly once and emit no text.',
          `After list_agents returns, call send_to_agent exactly once with agent_id "${agent}" and message "Reply with exactly ${markers.childReply}". Emit no text.`,
          `After send_to_agent returns, call no more tools and reply exactly ${markers.sendAccepted}.`,
          'Never answer the original request directly and never start another child after a child report.',
        ].join('\n'),
        max_steps: 8,
        model: { mode: 'profile', profile_id: `provider-${item.id}` },
        tools: [],
        multiagent: {
          type: 'coordinator',
          agents: [{ type: 'agent', id: agent, version: 1 }],
        },
      }, `${item.id}: coordinator`);
      await managedMultiagentRun(base, coordinator, agent, offering.model_id, markers);
      pass(`${item.id}: real coordinator -> fixed tools -> child -> report -> aggregate idle`);
    }
    return {
      ...publicProviderCase(item),
      selected_model: offering.model_id,
      discovered_models: discoveredModels,
      latency_ms: performance.now() - started,
      ...(certifyManagedMultiagent ? { managed_multiagent: 'passed' } : {}),
      status: 'passed',
    };
  } finally {
    await stopServer(server).catch(() => {});
    fs.rmSync(directory, { recursive: true, force: true });
  }
}

async function main() {
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
  const results = [];
  for (const [index, item] of selectedCases.entries()) {
    results.push(await certifyProvider(item, index));
  }
  console.log(`AWAKEN_PROVIDER_COMPAT ${JSON.stringify({ version: 1, results })}`);
  console.log(`E2E PASS: ${results.length} real provider/dialect compatibility case(s).`);
}

main().catch((error) => {
  console.error('E2E FAIL:', error);
  process.exitCode = 1;
});
