// Black-box ADR-0071 driver. Every operation uses the one public Ingress URL;
// private Control/Coordinator/Worker addresses are intentionally unknowable here.
import assert from 'node:assert/strict';
import Anthropic from '@anthropic-ai/sdk';
// The shared E2E harness is intentionally JavaScript-owned and has no parallel
// declaration shim; Node executes this import directly in the K3D lane.
// @ts-expect-error -- harness.mjs is the canonical runtime implementation.
import { waitForSessionEventReceipt, waitForValue } from '../harness.mjs';

const MANAGED_BETA = 'managed-agents-2026-04-01';
const BETAS = [MANAGED_BETA];
const WORKSPACE = 'workspace-adr71';
const AGENT = 'adr71-agent';

function clientFor(baseURL: string) {
  return new Anthropic({ apiKey: 'e2e-dummy', baseURL, maxRetries: 0 });
}

function betaFor(route: string) {
  if (route.startsWith('/v1/memory_stores')) return 'agent-memory-2026-07-22';
  if (route.startsWith('/v1/skills')) return 'skills-2025-10-02';
  return MANAGED_BETA;
}

const RAW_ROUTE_ROOTS = [
  '/internal',
  '/v1/config',
  '/v1/deployments',
  '/v1/files',
  '/v1/memory_stores',
  '/v1/skills',
];

// Raw-boundary cause/effect table:
// D1 Config/Deployment/File/Skill/Memory/internal route -> raw request is the
// independent management, multipart, fault, or isolation oracle;
// D2 Session, Environment, Model, or any other compatible SDK route -> reject
// before I/O so Managed Session traffic cannot regain a parallel fetch path.
// Constraint K1: an allowed root must end at a path-segment boundary.
function assertRawRoute(route: string) {
  const pathname = new URL(route, 'http://raw-route.invalid').pathname;
  const allowed = RAW_ROUTE_ROOTS.some(
    (root) => pathname === root || pathname.startsWith(`${root}/`),
  );
  assert.ok(allowed, `raw API route is outside its test-only boundary: ${pathname}`);
}

async function rawApi(base: string, method: string, route: string, body?: unknown) {
  assertRawRoute(route);
  const isForm = body instanceof FormData;
  const response = await fetch(`${base}${route}`, {
    method,
    headers: {
      'anthropic-beta': betaFor(route),
      ...(body === undefined || isForm ? {} : { 'content-type': 'application/json' }),
    },
    body: body === undefined ? undefined : isForm ? body : JSON.stringify(body),
  });
  const text = await response.text();
  let value: any = null;
  try { value = text ? JSON.parse(text) : null; } catch { value = text; }
  return { status: response.status, value, text };
}

async function expectRawStatus(
  base: string,
  method: string,
  route: string,
  status: number,
  body?: unknown,
) {
  const result = await rawApi(base, method, route, body);
  assert.equal(result.status, status, `${method} ${route}: ${JSON.stringify(result.value)}`);
  return result.value;
}

async function expectRawIdempotentStatusEventually(
  base: string,
  method: string,
  route: string,
  status: number,
  body?: unknown,
  timeoutMs = 20_000,
) {
  assert.ok(
    method === 'GET' || (method === 'POST' && route.endsWith('/publish')),
    `retry helper refuses a non-idempotent command: ${method} ${route}`,
  );
  const deadline = Date.now() + timeoutMs;
  let result = await rawApi(base, method, route, body);
  while (Date.now() < deadline && [502, 503, 504].includes(result.status)) {
    await new Promise((resolve) => setTimeout(resolve, 200));
    result = await rawApi(base, method, route, body);
  }
  assert.equal(result.status, status, `${method} ${route}: ${JSON.stringify(result.value)}`);
  return result.value;
}

function retryableReadStatus(error: unknown) {
  const status = (error as { status?: unknown })?.status;
  if (typeof status === 'number') {
    return [408, 409, 429, 500, 502, 503, 504].includes(status) ? status : null;
  }
  const name = (error as { name?: unknown })?.name;
  return name === 'APIConnectionError' || name === 'APIConnectionTimeoutError'
    ? String(name)
    : null;
}

async function retrieveSessionProbe(client: Anthropic, sessionId: string) {
  const started = performance.now();
  try {
    const session = await client.beta.sessions.retrieve(sessionId, { betas: BETAS });
    return { session, status: 200 as number | string, elapsed: performance.now() - started };
  } catch (error) {
    const status = retryableReadStatus(error);
    if (status === null) throw error;
    return { session: null, status, elapsed: performance.now() - started };
  }
}

async function expectStableSession(
  client: Anthropic,
  sessionId: string,
  consecutive = 5,
  timeoutMs = 20_000,
) {
  const deadline = performance.now() + timeoutMs;
  let stable = 0;
  let last = 'no read completed';
  while (performance.now() < deadline) {
    const result = await retrieveSessionProbe(client, sessionId);
    last = `status=${result.status} elapsed_ms=${result.elapsed.toFixed(1)}`;
    stable = result.session ? stable + 1 : 0;
    if (result.session && stable >= consecutive) return result.session;
    await new Promise((resolve) => setTimeout(resolve, 200));
  }
  assert.fail(`Session ${sessionId} did not remain readable for ${consecutive} probes: ${last}`);
}

async function awaitResponsiveCoordinatorSet(
  client: Anthropic,
  sessionIds: string[],
  timeoutMs = 30_000,
  maxReadMs = 2_000,
) {
  const deadline = performance.now() + timeoutMs;
  let last = 'no probe completed';
  while (performance.now() < deadline) {
    // Use two reads per Session so the public edge opens enough upstream
    // connections to exercise both replicated Coordinator pools without naming
    // or calling a private Pod. Reads are repeatable and carry no business effect.
    const probes = sessionIds.flatMap((sessionId) => [sessionId, sessionId]);
    const results = await Promise.all(
      probes.map((sessionId) => retrieveSessionProbe(client, sessionId)),
    );
    const slowest = Math.max(...results.map((result) => result.elapsed));
    const statuses = [...new Set(results.map((result) => result.status))];
    last = `statuses=${statuses.join(',')} slowest_ms=${slowest.toFixed(1)}`;
    if (statuses.length === 1 && statuses[0] === 200 && slowest <= maxReadMs) return;
    await new Promise((resolve) => setTimeout(resolve, 250));
  }
  assert.fail(`replicated Coordinator pools did not converge: ${last}`);
}

async function uploadFile(base: string, marker: string) {
  const form = new FormData();
  form.append('purpose', 'agent');
  form.append('file', new Blob([marker]), 'adr71-input.txt');
  const response = await rawApi(base, 'POST', '/v1/files', form);
  assert.equal(response.status, 200, `POST /v1/files: ${response.text}`);
  return response.value;
}

async function uploadSkill(base: string, name: string, content: string) {
  const form = new FormData();
  form.append('display_title', name);
  form.append(
    'files[]',
    new Blob([
      `---\nname: ${name}\ndescription: distributed Worker skill proof\nenvironment: filesystem\n---\n${content}`,
    ], { type: 'text/markdown' }),
    'SKILL.md',
  );
  const response = await rawApi(base, 'POST', '/v1/skills', form);
  assert.equal(response.status, 200, `POST /v1/skills: ${response.text}`);
  return response.value;
}

async function listSessionEvents(client: Anthropic, sessionId: string) {
  const history = [];
  for await (const event of client.beta.sessions.events.list(sessionId, { betas: BETAS })) {
    history.push(event);
  }
  return history;
}

function hasCompletedProviderMarker(events: any[], marker: string) {
  const expected = `ADR71-PROVIDER:${marker}`;
  const markerIndex = events.findIndex((event) => event.type === 'agent.message'
    && (event.content ?? []).some(
      (block: any) => block.type === 'text' && block.text === expected,
    ));
  return markerIndex >= 0
    && events.slice(markerIndex + 1).some((event) => event.type === 'session.status_idle');
}

async function waitForInitialAgentMarker(
  client: Anthropic,
  sessionId: string,
  marker: string,
  timeoutMs = 180_000,
) {
  // Deployment initial_events exception table:
  // I1 Deployment run creates a Session with initial_events but exposes no
  // independent Event receipt -> the official SDK lists bounded durable history;
  // I2 exact unique Provider marker followed by idle -> the initial turn completed;
  // I3 marker/idle absent at the deadline -> fail without inventing a receipt or
  // replaying Deployment run. Constraint K1: every ordinary Event send uses the
  // receipt-scoped helper below instead.
  return waitForValue(
    () => listSessionEvents(client, sessionId),
    (events: any[]) => hasCompletedProviderMarker(events, marker),
    `Deployment initial Event ${marker} to commit its Provider marker and idle`,
    { timeoutMs, pollMs: 500 },
  );
}

async function sendAndWaitForAgentMarker(
  client: Anthropic,
  sessionId: string,
  marker: string,
  timeoutMs = 180_000,
) {
  // Shared positive-send decision table:
  // S1 one official SDK send returns one exact User Event receipt -> observe only
  // history after that receipt; S2 its exact Provider marker then idle commits ->
  // return the full history and end-to-end elapsed time; S3 missing receipt or
  // deadline -> fail. K1 maxRetries=0 and no caller replays this non-idempotent
  // write; K2 latency starts before send and ends only after marker plus idle.
  const started = performance.now();
  const receipt = await client.beta.sessions.events.send(sessionId, {
    events: [{ type: 'user.message', content: [{ type: 'text', text: marker }] }],
    betas: BETAS,
  });
  assert.equal(receipt.data?.length, 1, `one exact Event receipt for ${marker}`);
  const receiptId = receipt.data?.[0]?.id;
  assert.equal(typeof receiptId, 'string', `SDK receipt id for ${marker}`);
  const observation = await waitForSessionEventReceipt(
    client,
    sessionId,
    receiptId,
    BETAS,
    ({ delta }: { delta: any[] }) => hasCompletedProviderMarker(delta, marker),
    `Event ${marker} receipt to reach its exact Provider marker and later idle`,
    { timeoutMs, pollMs: 500 },
  );
  return { ...observation, elapsed: performance.now() - started };
}

function providerResponseCounts(events: any[]) {
  const responses = events
    .filter((event: any) => event.type === 'agent.message')
    .flatMap((event: any) => event.content ?? [])
    .filter((block: any) => block.type === 'text' && block.text.startsWith('ADR71-PROVIDER:'))
    .map((block: any) => block.text);
  const counts = new Map<string, number>();
  for (const response of responses) counts.set(response, (counts.get(response) ?? 0) + 1);
  return counts;
}

function assertProviderResponseCount(events: any[], marker: string) {
  assert.equal(
    providerResponseCounts(events).get(`ADR71-PROVIDER:${marker}`),
    1,
    `expected one exact Provider response for ${marker}`,
  );
}

function assertProviderResponsesAreUnique(events: any[]) {
  const counts = providerResponseCounts(events);
  const duplicates = [...counts].filter(([, count]) => count !== 1);
  assert.deepEqual(duplicates, [], `duplicate Provider responses: ${JSON.stringify(duplicates)}`);
}

async function bootstrap(base: string) {
  // Cause/effect decision table:
  // B1 private route through the public endpoint -> 404 and no mutation;
  // B2 tenant Credential mutation under deployment-managed model supply -> typed
  // 403 without reflecting the submitted secret or creating a second supply path;
  // B3 File/Memory/Skill CRUD -> durable, secret-free public projections;
  // B4 Agent publication uses the deployment-supplied model and sealed Credential
  // candidate -> one immutable snapshot; B5 Deployment launch with initial_events
  // -> one Session but no independent Event receipt; B6 official SDK history
  // observes its unique Provider marker followed by idle; B7 real Worker
  // materializes all pins and commits one claim-fenced response.
  // Constraint K1: B5 is the sole no-receipt exception; it is never replayed.
  const client = clientFor(base);
  await expectRawStatus(base, 'POST', '/internal/v1/executable-agents/withdraw', 404, {
    workspace_id: 'auth-probe', agent_id: 'auth-probe', lifecycle_revision: 1,
  });
  await expectRawStatus(base, 'GET', '/v1/config/catalog', 200);
  for await (const _model of client.beta.models.list({ betas: BETAS })) break;

  const credentialSecret = 'adr71-must-never-be-returned'; // awaken-allow: secret
  const deniedCredential = await expectRawStatus(
    base,
    'POST',
    '/v1/config/credentials',
    403,
    {
      workspace_id: WORKSPACE, kind: 'vault', provider_id: 'anthropic',
      env_key: 'ADR71_PUBLIC_PROBE', secret: credentialSecret,
    },
  );
  assert.equal(deniedCredential.code, 'model_supply_managed', JSON.stringify(deniedCredential));
  assert.ok(
    !JSON.stringify(deniedCredential).includes(credentialSecret),
    'credential rejection leaked secret',
  );

  const fileMarker = 'ADR71-FILE-MATERIALIZED';
  const file = await uploadFile(base, fileMarker);
  assert.equal(file.downloadable, false, JSON.stringify(file));
  await expectRawStatus(base, 'GET', `/v1/files/${file.id}/content`, 400);

  const memory = await expectRawStatus(base, 'POST', '/v1/memory_stores', 200, {
    name: 'adr71-memory', description: 'distributed Worker memory proof',
  });
  await expectRawStatus(base, 'GET', `/v1/memory_stores/${memory.id}/config`, 404);
  await expectRawStatus(base, 'POST', `/v1/memory_stores/${memory.id}/memories`, 200, {
    path: '/fact.md', content: 'ADR71-MEMORY-MATERIALIZED',
  });

  const skill = await uploadSkill(base, 'adr71-skill', 'ADR71-SKILL-MATERIALIZED');

  const agentId = 'adr71-agent';
  await expectRawStatus(base, 'PUT', `/v1/config/agents/${agentId}`, 200, {
    name: 'ADR-0071 distributed Agent',
    system: 'Answer the user through the configured provider.',
    model: { id: 'adr71-echo' }, tools: [],
    skills: [{ type: 'custom', skill_id: skill.id, version: '1' }],
  });
  await expectRawStatus(base, 'PUT', `/v1/config/agents/${agentId}/resources`, 200, {
    agent_id: agentId,
    revision: 1,
    inputs: [
      {
        binding_id: 'adr71-file', target: { kind: 'file', id: file.id },
        mount_path: '/inputs/adr71-input.txt', access: 'read_only',
      },
      {
        binding_id: 'adr71-memory', target: { kind: 'memory_store', id: memory.id },
        mount_path: '/memory', access: 'read_write',
      },
    ],
  });
  const publication = await expectRawStatus(
    base, 'POST', `/v1/config/agents/${agentId}/publish`, 200,
  );
  assert.ok(publication.fingerprint, JSON.stringify(publication));

  const environment = await client.beta.environments.create({
    name: 'adr71-distributed', config: { type: 'cloud' },
    betas: BETAS,
  });
  const initialMarker = 'ADR71-INITIAL-RESPONSE';
  const deployment = await expectRawStatus(base, 'POST', '/v1/deployments', 200, {
    agent: agentId, environment_id: environment.id, name: 'adr71-deployment',
    initial_events: [{
      type: 'user.message', content: [{ type: 'text', text: initialMarker }],
    }],
  });
  const run = await expectRawStatus(
    base, 'POST', `/v1/deployments/${deployment.id}/run`, 200,
  );
  assert.equal(run.error, null, JSON.stringify(run));
  assert.ok(run.session_id, JSON.stringify(run));
  const initialHistory = await waitForInitialAgentMarker(client, run.session_id, initialMarker);
  assertProviderResponseCount(initialHistory, initialMarker);
  assertProviderResponsesAreUnique(initialHistory);
  const restoredMemory = await expectRawStatus(
    base, 'GET', `/v1/memory_stores/${memory.id}`, 200,
  );
  assert.equal(restoredMemory.id, memory.id);
  console.log(
    `OK ${deployment.id} ${run.session_id} ${environment.id} ${file.id} ${memory.id} ${skill.id}`,
  );
}

async function unavailablePublication(base: string) {
  // U1 both Coordinator replicas unavailable after Control persistence -> the
  // single gateway reports a retryable 502/503/504 according to whether endpoint
  // removal, connect refusal, or timeout wins the network race. Every rule fails
  // closed: the exact StoredPublication stays retryable and no local registration
  // path or success response exists.
  const agentId = 'adr71-recovery-agent';
  await expectRawStatus(base, 'PUT', `/v1/config/agents/${agentId}`, 200, {
    name: 'ADR-0071 recovery Agent', system: 'Recover registration.',
    model: { id: 'adr71-echo' }, tools: [],
  });
  const unavailable = await rawApi(base, 'POST', `/v1/config/agents/${agentId}/publish`);
  assert.ok(
    [502, 503, 504].includes(unavailable.status),
    `publication must fail retryably, got ${unavailable.status}: ${unavailable.text}`,
  );
  console.log('OK unavailable publication remained retryable');
}

async function retryPublication(base: string) {
  // U2 the same publication after Coordinator recovery -> acknowledged once by
  // the canonical registrar and exposed through the same public endpoint. Pod
  // readiness may precede Service endpoint propagation, so retry only the same
  // idempotent public command on retryable gateway/service statuses.
  const publication = await expectRawIdempotentStatusEventually(
    base, 'POST', '/v1/config/agents/adr71-recovery-agent/publish', 200,
  );
  assert.ok(publication.fingerprint, JSON.stringify(publication));
  console.log('OK publication registration recovered');
}

async function verifyDurable(
  base: string,
  deploymentId: string,
  sessionId: string,
  marker = 'ADR71-AFTER-FAILURE',
) {
  // Cause/effect decision table:
  // R1 authority replacement/failover + stable edge route -> durable Deployment
  // and Session remain readable; a cold Coordinator projection must never have
  // a process-local sandbox binding to adopt because Worker placement was frozen
  // before realization;
  // R2 EndpointSlice convergence is uncertain -> use repeatable GET probes until
  // stable, then issue one official SDK Event send with maxRetries=0;
  // R3 its exact receipt is processed before the exact Provider marker and later
  // idle -> a live Worker committed exactly one completed response;
  // R4 every prior Provider marker is also unique, so an earlier ambiguous write
  // cannot be hidden by a later successful read.
  const client = clientFor(base);
  const deployment = await expectRawIdempotentStatusEventually(
    base, 'GET', `/v1/deployments/${deploymentId}`, 200, undefined, 60_000,
  );
  assert.equal(deployment.id, deploymentId, JSON.stringify(deployment));
  const session = await expectStableSession(client, sessionId, 5, 60_000);
  assert.equal(session.id, sessionId, JSON.stringify(session));
  const { events } = await sendAndWaitForAgentMarker(client, sessionId, marker);
  assertProviderResponseCount(events, marker);
  assertProviderResponsesAreUnique(events);
  console.log(`OK durable flow ${marker}`);
}

async function createParallelSessions(client: Anthropic, environmentId: string, count: number) {
  return Promise.all(Array.from({ length: count }, async () => {
    const session = await client.beta.sessions.create({
      agent: AGENT,
      environment_id: environmentId,
      betas: BETAS,
    });
    assert.ok(session.id, JSON.stringify(session));
    return session.id as string;
  }));
}

async function batch(
  base: string,
  originalSessionId: string,
  environmentId: string,
  prefix: string,
  count: number,
) {
  // C1 one request per independent Session + one or more process failures ->
  // both Workers carry real parallel claims; C2 each maxRetries=0 SDK send returns
  // one exact receipt and only its marker+later idle terminates that rule;
  // C3 claim fencing -> no duplicate Provider response after recovery/reclaim;
  // C4 the original durable Session remains one member of the same batch.
  const client = clientFor(base);
  const markers = Array.from({ length: count }, (_, index) => `${prefix}-${index}`);
  const sessions = [
    originalSessionId,
    ...await createParallelSessions(client, environmentId, Math.max(0, count - 1)),
  ];
  const observations = await Promise.all(markers.map((marker, index) =>
    sendAndWaitForAgentMarker(client, sessions[index], marker, 240_000)));
  for (let index = 0; index < markers.length; index += 1) {
    const marker = markers[index];
    assertProviderResponseCount(observations[index].events, marker);
    assertProviderResponsesAreUnique(observations[index].events);
  }
  console.log(`OK batch ${prefix} ${count}`);
}

async function load(base: string, environmentId: string) {
  // Cause/effect graph: C1 a new Resource-bearing K8s Session has no reusable
  // Pod because its projected mounts are Session-specific -> first execution
  // includes bounded Pod/material cold start; C2 every Session has completed one
  // cold start -> its live environment is resident; C3 bounded concurrent writes
  // below admission limits -> zero HTTP errors; C4 active/active Coordinator and
  // Worker dispatch -> unique Run ids and exactly-once outputs. Effects are E1 a
  // separately bounded cold-start result and E2 the steady-state completed-turn
  // p95. Every latency starts before one SDK send and ends only when that exact
  // receipt's Provider marker and later idle are durable. The admission response
  // alone is never treated as synchronous turn completion. Mixing C1 into E2
  // would make the steady-state contract depend on Kubelet image/sidecar startup.
  //
  // | Rule | Resource Pod resident | Concurrent load | Effect |
  // |---|---|---|---|
  // | L1 | no | one per Session | complete once within cold-start bound |
  // | L2 | yes + DB failover pools not converged | read-only public probes |
  // |    | expose recovery latency; never start the steady-state timer |
  // | L3 | yes + every replica responsive | bounded receipt-scoped turns | zero
  // |    | errors and marker+idle completion p95 <= configured limit |
  // | L4 | yes | bounded + active/active | every full-history marker exactly once |
  const count = Number(process.env.ADR71_LOAD_REQUESTS ?? 48);
  const concurrency = Number(process.env.ADR71_LOAD_CONCURRENCY ?? 12);
  assert.ok(
    Number.isInteger(count) && Number.isInteger(concurrency)
      && count > 0 && concurrency > 0 && concurrency <= count,
    `invalid load shape requests=${count} concurrency=${concurrency}`,
  );
  // Five seconds is a performance regression gate for the complete
  // public->Coordinator->claim->Worker->Provider->commit->idle round trip, not
  // an admission SLO.
  const p95Limit = Number(process.env.ADR71_TURN_P95_MS ?? 5_000);
  const coldStartLimit = Number(process.env.ADR71_COLD_START_MAX_MS ?? 30_000);
  const markers = Array.from({ length: count }, (_, index) => `ADR71-LOAD-${Date.now()}-${index}`);
  const managed = clientFor(base);
  const sessions = await createParallelSessions(managed, environmentId, concurrency);
  const coldMarkers = sessions.map((_, index) => `ADR71-COLD-${Date.now()}-${index}`);
  const coldObservations = await Promise.all(sessions.map((sessionId, index) =>
    sendAndWaitForAgentMarker(managed, sessionId, coldMarkers[index], coldStartLimit)));
  const coldElapsed = Math.max(...coldObservations.map((observation) => observation.elapsed));
  assert.ok(
    coldElapsed <= coldStartLimit,
    `parallel Resource Pod cold start ${coldElapsed.toFixed(1)}ms > ${coldStartLimit}ms`,
  );
  const histories = coldObservations.map((observation) => observation.events);
  coldObservations.forEach((observation, index) => {
    assertProviderResponseCount(observation.events, coldMarkers[index]);
    assertProviderResponsesAreUnique(observation.events);
  });
  // The promoted standby preserves accepted facts, but each replicated SQLx pool
  // discovers its old idle socket independently. Converge them with repeatable
  // public reads before measuring the separate steady-state SLO; using one
  // successful write here would warm only its randomly selected Coordinator.
  await awaitResponsiveCoordinatorSet(managed, sessions);
  const latencies: number[] = [];
  async function runClient(clientIndex: number) {
    for (let index = clientIndex; index < markers.length; index += concurrency) {
      const observation = await sendAndWaitForAgentMarker(
        managed, sessions[clientIndex], markers[index], 300_000,
      );
      latencies.push(observation.elapsed);
      histories[clientIndex] = observation.events;
    }
  }
  await Promise.all(Array.from({ length: concurrency }, (_, index) => runClient(index)));
  latencies.sort((left, right) => left - right);
  const p50 = latencies[Math.min(latencies.length - 1, Math.ceil(latencies.length * 0.50) - 1)];
  const p95 = latencies[Math.min(latencies.length - 1, Math.ceil(latencies.length * 0.95) - 1)];
  const max = latencies[latencies.length - 1];
  assert.ok(
    p95 <= p95Limit,
    `completed turn p95 ${p95.toFixed(1)}ms > ${p95Limit}ms `
      + `(p50=${p50.toFixed(1)}ms max=${max.toFixed(1)}ms)`,
  );
  const markersBySession = sessions.map(() => [] as string[]);
  markers.forEach((marker, index) => markersBySession[index % concurrency].push(marker));
  for (let index = 0; index < sessions.length; index += 1) {
    for (const marker of markersBySession[index]) {
      assertProviderResponseCount(histories[index], marker);
    }
    assertProviderResponsesAreUnique(histories[index]);
  }
  console.log(
    `OK load requests=${count} concurrency=${concurrency} cold_ms=${coldElapsed.toFixed(1)} `
      + `turn_p50_ms=${p50.toFixed(1)} turn_p95_ms=${p95.toFixed(1)} turn_max_ms=${max.toFixed(1)}`,
  );
}

async function main() {
  const [mode, base, ...args] = process.argv.slice(2);
  switch (mode) {
    case 'bootstrap': return bootstrap(base);
    case 'unavailable-publication': return unavailablePublication(base);
    case 'retry-publication': return retryPublication(base);
    case 'verify-durable': return verifyDurable(base, args[0], args[1], args[2]);
    case 'batch': return batch(base, args[0], args[1], args[2], Number(args[3]));
    case 'load': return load(base, args[0]);
    default: throw new Error(`unknown mode ${mode}`);
  }
}

main().catch((error) => {
  console.error(error);
  process.exitCode = 1;
});
