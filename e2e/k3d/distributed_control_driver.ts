// Black-box ADR-0071 driver. Every operation uses the one public Ingress URL;
// private Control/Coordinator/Worker addresses are intentionally unknowable here.
// The independent Provider double's private control URL is a fault injector,
// never a product read/write path or a production termination receipt.
import assert from 'node:assert/strict';
import { resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import Anthropic from '@anthropic-ai/sdk';
// The shared E2E harness is intentionally JavaScript-owned and has no parallel
// declaration shim; Node executes this import directly in the K3D lane.
// @ts-expect-error -- harness.mjs is the canonical runtime implementation.
import {
  listManagedSessionEvents,
  managedFileUploadForm,
  retryableReadStatus,
  waitForSessionEventReceipt,
  waitForValue,
} from '../harness.mjs';

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

async function awaitResponsivePublicReads(
  client: Anthropic,
  sessionIds: string[],
  timeoutMs = 30_000,
  maxReadMs = 2_000,
) {
  const deadline = performance.now() + timeoutMs;
  let last = 'no probe completed';
  while (performance.now() < deadline) {
    // Repeated public reads bound observed recovery latency without business
    // effects. Load balancing does not prove every replica was selected; this
    // is a public-read precondition, not per-replica pool-convergence evidence.
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
  assert.fail(`public Session reads did not recover: ${last}`);
}

async function uploadFile(base: string, marker: string) {
  const response = await rawApi(
    base,
    'POST',
    '/v1/files',
    managedFileUploadForm(marker, 'adr71-input.txt'),
  );
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

export function hasCompletedProviderMarker(events: any[], marker: string) {
  const expected = `ADR71-PROVIDER:${marker}`;
  const markerIndex = events.findIndex((event) => event.type === 'agent.message'
    && (event.content ?? []).some(
      (block: any) => block.type === 'text' && block.text === expected,
    ));
  return markerIndex >= 0
    && events.slice(markerIndex + 1).some((event) => event.type === 'session.status_idle'
      && event.stop_reason?.type === 'end_turn');
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
    () => listManagedSessionEvents(client, sessionId, BETAS),
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
  const acceptedAt = performance.now();
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
    {
      timeoutMs,
      pollMs: 500,
      // Failover receipt observation is idempotent. A replacement replica may
      // correctly reject one mixed committed-prefix read with 503; retry that
      // read under the same exact receipt, never the Event send above.
      retryReadError: (error: unknown) => retryableReadStatus(error) !== null,
      preferStream: true,
    },
  );
  const observedAt = performance.now();
  return { ...observation, elapsed: observedAt - started, admissionMs: acceptedAt - started, observationMs: observedAt - acceptedAt };
}

// Pressure qualification and retained-cluster diagnostics share this exact
// receipt observer; the wrapper only binds this scenario's marker predicate and
// stream-first transport to the canonical harness state machine.
export async function waitForRecentSessionEventReceipt(
  client: Anthropic,
  sessionId: string,
  receiptId: string,
  marker: string,
  timeoutMs: number,
) {
  return waitForSessionEventReceipt(
    client,
    sessionId,
    receiptId,
    BETAS,
    ({ delta }: { delta: any[] }) => hasCompletedProviderMarker(delta, marker),
    `Event ${marker} receipt to reach its exact Provider marker and later idle`,
    {
      timeoutMs,
      pollMs: 500,
      retryReadError: (error: unknown) => retryableReadStatus(error) !== null,
      preferStream: true,
    },
  );
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

export function nativeCrashHistoryErrors(
  events: any[],
  marker: string,
  requireIdle: boolean,
) {
  // Crash-observation decision table: N1 zero Provider markers => execution did
  // not cross the terminal model boundary; N2 two markers => replay occurred;
  // N3 one marker without later idle => the terminal commit exists but queue
  // settlement is still blocked; N4 one marker plus later idle => successor
  // completed the existing lifecycle. Preparation accepts N3/N4, while final
  // verification accepts only N4. Older history is excluded by the caller's
  // receipt-scoped delta, so it cannot satisfy either phase.
  const errors: string[] = [];
  const count = providerResponseCounts(events).get(`ADR71-PROVIDER:${marker}`) ?? 0;
  if (count !== 1) errors.push(`Provider response count ${count} != 1`);
  if (requireIdle && !hasCompletedProviderMarker(events, marker)) {
    errors.push('Provider response has no later idle event');
  }
  return errors;
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

async function readDurableDuringFault(
  base: string,
  deploymentId: string,
  sessionId: string,
  committedMarker: string,
) {
  // Node-partition cause/effect table:
  // C1 one non-database k3d node is unreachable; C2 the replicated public edge
  // remains stable; C3 no write is issued while the exact Session executor is
  // unobservable. R1=C1+C2+C3 => the committed Deployment and Session remain
  // readable with their exact identities and event set. R2=!C2 => bounded
  // failure. R3 never retries a write, so an ambiguous transport result cannot
  // manufacture a duplicate Event while the node is partitioned.
  const client = clientFor(base);
  const deployment = await expectRawIdempotentStatusEventually(
    base, 'GET', `/v1/deployments/${deploymentId}`, 200, undefined, 60_000,
  );
  assert.equal(deployment.id, deploymentId);
  const session = await client.beta.sessions.retrieve(sessionId, { betas: BETAS });
  assert.equal(session.id, sessionId);
  const events = await listManagedSessionEvents(client, sessionId, BETAS);
  const eventIds = events.map((event: any) => event.id);
  assert.equal(new Set(eventIds).size, eventIds.length, 'committed event ids stay unique');
  assertProviderResponseCount(events, committedMarker);
  assertProviderResponsesAreUnique(events);
  console.log(`OK read-only partition observation events=${eventIds.length} marker=${committedMarker}`);
}

async function prepareNativeCrash(base: string, environmentId: string, marker: string) {
  // The final k3d phase must use a genuinely closed Native surface: no tools,
  // Skills, Resources or authored plugins. One public Event receipt anchors the
  // command and is never resent. The shell injects failure only after the one
  // Provider call and the old Worker's rejected finish prove that Runtime
  // returned after committing its response. Public history is intentionally not
  // awaited here: its projection follows settlement, which this fault holds.
  const agentId = 'adr71-native-crash-agent';
  await expectRawStatus(base, 'PUT', `/v1/config/agents/${agentId}`, 200, {
    name: 'ADR-0071 Native crash recovery Agent',
    system: 'Return the configured provider response.',
    model: { id: 'adr71-echo' },
    tools: [],
    skills: [],
  });
  await expectRawStatus(base, 'POST', `/v1/config/agents/${agentId}/publish`, 200);
  const client = clientFor(base);
  const session = await client.beta.sessions.create({
    agent: agentId,
    environment_id: environmentId,
    betas: BETAS,
  });
  const receipt = await client.beta.sessions.events.send(session.id, {
    events: [{ type: 'user.message', content: [{ type: 'text', text: marker }] }],
    betas: BETAS,
  });
  assert.equal(receipt.data?.length, 1, 'one Native crash Event receipt');
  const receiptId = receipt.data?.[0]?.id;
  assert.equal(typeof receiptId, 'string', 'Native crash Event receipt id');
  console.log(`OK ${session.id} ${receiptId}`);
  return { sessionId: session.id, receiptId: receiptId as string };
}

export function nativeProviderInvocationErrors(value: any, marker: string, completed: boolean) {
  // F1 one independent accepted/started invocation; F2 running or completed
  // according to the explicit fault phase; F3 exact retained output after
  // completion. Any duplicate, missing, wrong identity or partial outcome fails.
  const errors: string[] = [];
  if (value?.marker !== marker) errors.push('wrong Provider fixture identity');
  if (value?.accepted !== 1 || value?.started !== 1) errors.push('not one actual Provider invocation');
  if (value?.running !== Number(!completed) || value?.completed !== Number(completed)) errors.push('wrong Provider execution phase');
  const expected = completed ? [`ADR71-PROVIDER:${marker}`] : [];
  if (JSON.stringify(value?.results) !== JSON.stringify(expected)) errors.push('wrong retained Provider result');
  return errors;
}

async function observeNativeProvider(base: string, marker: string, completed: boolean) {
  const value = await waitForValue(async () => {
    const response = await fetch(`${base}/__scenario/invocations/${encodeURIComponent(marker)}`, {
      headers: { 'x-api-key': 'adr71-provider-secret' }, signal: AbortSignal.timeout(5000), // awaken-allow: secret
    });
    if (response.status === 404) return null;
    assert.equal(response.status, 200, 'private Provider fixture observation');
    return response.json();
  }, (value: any) => nativeProviderInvocationErrors(value, marker, completed).length === 0,
  'one exact Provider double invocation reaches the explicit fault phase', {
    timeoutMs: 20_000, pollMs: 100,
    retryReadError: (error: any) => error?.name === 'TypeError' && error?.message === 'fetch failed',
  });
  assert.deepEqual(nativeProviderInvocationErrors(value, marker, completed), []);
  console.log(`OK Provider ${marker} completed=${completed}`);
}

async function releaseNativeProvider(base: string, marker: string) {
  // One idempotent fault command releases already-accepted work. It never
  // issues another inference or imports this mock observation into Runtime.
  const response = await fetch(`${base}/__scenario/invocations/${encodeURIComponent(marker)}/release`, {
    method: 'POST', headers: { 'x-api-key': 'adr71-provider-secret' }, signal: AbortSignal.timeout(5000), // awaken-allow: secret
  });
  assert.equal(response.status, 202, 'release accepted Provider work');
  await observeNativeProvider(base, marker, true);
}

async function verifyNativeInFlight(
  base: string, sessionId: string, receiptId: string, marker: string,
) {
  // U1 original dispatched call has no committed return after Worker death;
  // U2 its physical slot remains occupied; U3 a successor runs independent
  // work. SQL/Pod oracles in the same shell own U1/U2; SDK history owns no
  // synthesized successful result or completion. Error outcomes may settle
  // logically while physical execution remains Unknown; processed_at alone
  // is therefore not a quiescence predicate. Never resend the original Event.
  const [environmentId] = await createFleetEnvironments(clientFor(base), 1);
  const independent = await prepareNativeCrash(base, environmentId, 'ADR71-NATIVE-INDEPENDENT');
  await verifyNativeCrash(base, independent.sessionId, independent.receiptId, 'ADR71-NATIVE-INDEPENDENT');
  const history = await listManagedSessionEvents(clientFor(base), sessionId, BETAS);
  assert.deepEqual(nativeInFlightHistoryErrors(history, receiptId, marker), [], 'U1 exact unresolved command history');
  console.log(`OK Native unknown retained ${sessionId} ${receiptId}`);
}

export function nativeInFlightHistoryErrors(history: any[], receiptId: string, marker: string) {
  const errors: string[] = [];
  const ids = history.map((event: any) => event.id);
  if (new Set(ids).size !== ids.length) errors.push('repeated public facts');
  const accepted = history.findIndex((event: any) => event.id === receiptId);
  if (accepted < 0) errors.push('original Event receipt absent');
  if ((providerResponseCounts(history).get(`ADR71-PROVIDER:${marker}`) ?? 0) !== 0) errors.push('invented Provider result');
  if (accepted >= 0 && history.slice(accepted + 1).some((event: any) =>
    event.type === 'session.status_idle' && event.stop_reason?.type === 'end_turn')) errors.push('invented success');
  return errors;
}

async function verifyNativeCrash(
  base: string,
  sessionId: string,
  receiptId: string,
  marker: string,
) {
  // No command is resent here. The successor must consume the original durable
  // Run, exact process-stop receipt and terminal ThreadCommit, then publish idle
  // without a second Provider response.
  const client = clientFor(base);
  const observation = await waitForSessionEventReceipt(
    client,
    sessionId,
    receiptId,
    BETAS,
    ({ delta }: { delta: any[] }) => nativeCrashHistoryErrors(delta, marker, true).length === 0,
    `Native crash Event ${marker} to settle after exact predecessor retirement`,
    { timeoutMs: 240_000, pollMs: 500, retryReadError: (error: unknown) => retryableReadStatus(error) !== null },
  );
  assert.deepEqual(nativeCrashHistoryErrors(observation.delta, marker, true), []);
  assertProviderResponsesAreUnique(observation.events);
  console.log(`OK Native crash recovery ${marker}`);
}

async function createParallelSessions(client: Anthropic, environmentIds: string[]) {
  return Promise.all(environmentIds.map(async (environmentId) => {
    const session = await client.beta.sessions.create({
      agent: AGENT,
      environment_id: environmentId,
      betas: BETAS,
    });
    assert.ok(session.id, JSON.stringify(session));
    return session.id as string;
  }));
}

export function validateFleetEnvironmentIds(environmentIds: string[], concurrency: number) {
  assert.equal(environmentIds.length, concurrency, 'fleet load requires one Environment per Session');
  assert.equal(new Set(environmentIds).size, concurrency, 'fleet load Environment ids must be unique');
  assert.ok(environmentIds.every((id) => typeof id === 'string' && id.length > 0),
    'fleet load Environment ids must be nonempty');
  return environmentIds;
}

async function createFleetEnvironments(client: Anthropic, concurrency: number) {
  const environments = await Promise.all(Array.from({ length: concurrency }, (_, index) =>
    client.beta.environments.create({
      name: `adr71-load-${index}`, config: { type: 'cloud' }, betas: BETAS,
    })));
  return validateFleetEnvironmentIds(
    environments.map((environment) => environment.id), concurrency,
  );
}

async function createFleetSessions(client: Anthropic, concurrency: number) {
  return createParallelSessions(client, await createFleetEnvironments(client, concurrency));
}

export async function disposeFixtureSessions(
  sessionIds: string[],
  dispose: (sessionId: string) => Promise<unknown>,
) {
  const results = await Promise.allSettled(sessionIds.map((sessionId) => dispose(sessionId)));
  const failures = results.flatMap((result, index) => result.status === 'rejected'
    ? [`${sessionIds[index]}: ${String(result.reason)}`]
    : []);
  assert.deepEqual(failures, [], 'every temporary batch Session accepted terminal disposal');
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
  // C4 the original durable Session remains one member of the same batch; C5
  // after those assertions, every auxiliary Session has no later-stage consumer.
  // E1 C5 publishes their ids for the shell fixture to observe exact Pod/PVC
  // identities before asking the public lifecycle owner to dispose them. The
  // original Session is deliberately excluded because stages 8 and 9 still use it.
  const client = clientFor(base);
  const markers = Array.from({ length: count }, (_, index) => `${prefix}-${index}`);
  const auxiliarySessions = await createParallelSessions(
    client, Array.from({ length: Math.max(0, count - 1) }, () => environmentId),
  );
  const sessions = [
    originalSessionId,
    ...auxiliarySessions,
  ];
  const observations = await Promise.all(markers.map((marker, index) =>
    sendAndWaitForAgentMarker(client, sessions[index], marker, 240_000)));
  for (let index = 0; index < markers.length; index += 1) {
    const marker = markers[index];
    assertProviderResponseCount(observations[index].events, marker);
    assertProviderResponsesAreUnique(observations[index].events);
  }
  console.log(`OK batch ${prefix} ${count} auxiliary=${auxiliarySessions.join(',')}`);
}

async function disposeBatch(base: string, encodedSessionIds: string) {
  const sessionIds = encodedSessionIds.split(',').filter(Boolean);
  assert.ok(sessionIds.length > 0, 'temporary batch Session ids are required');
  const client = clientFor(base);
  await disposeFixtureSessions(sessionIds, (sessionId) => client.beta.sessions.delete(
    sessionId, { betas: BETAS },
  ));
  console.log(`OK disposed auxiliary batch Sessions ${sessionIds.length}`);
}

export function auditLoadResults(
  histories: any[][],
  markersBySession: string[][],
  latencies: number[],
  p95Limit: number,
) {
  // L5 independent effects: performance and durable-history failures must all
  // be retained. A p95 breach cannot skip the exactly-once audit; a missing
  // sample or malformed latency cannot silently reduce the measured workload.
  const errors: string[] = [];
  const expected = markersBySession.flat().length;
  if (expected === 0 || latencies.length !== expected) errors.push(`completion samples ${latencies.length} != nonempty planned count ${expected}`);
  if (histories.length !== markersBySession.length) errors.push('history count differs from planned Sessions');
  for (let index = 0; index < markersBySession.length; index += 1) {
    const history = histories[index] ?? [];
    const counts = providerResponseCounts(history);
    for (const marker of markersBySession[index]) {
      try {
        assert.equal(
          counts.get(`ADR71-PROVIDER:${marker}`),
          1,
          `expected one exact Provider response for ${marker}`,
        );
      }
      catch (error) { errors.push(String(error)); }
    }
    try {
      const duplicates = [...counts].filter(([, count]) => count !== 1);
      assert.deepEqual(duplicates, [], `duplicate Provider responses: ${JSON.stringify(duplicates)}`);
    }
    catch (error) { errors.push(String(error)); }
  }
  const sorted = [...latencies].sort((left, right) => left - right);
  const percentile = (fraction: number) => sorted[Math.ceil(sorted.length * fraction) - 1] ?? null;
  const p50 = percentile(0.50);
  const p95 = percentile(0.95);
  const max = sorted.at(-1) ?? null;
  if (!Number.isFinite(p95Limit) || p95Limit <= 0 || latencies.some((value) => !Number.isFinite(value) || value < 0)) {
    errors.push('invalid latency measurement or performance limit');
  } else if (p95 !== null && p95 > p95Limit) {
    errors.push(`completed turn p95 ${p95.toFixed(1)}ms > ${p95Limit}ms (p50=${p50}ms max=${max}ms)`);
  }
  return { errors, expected, completed: latencies.length, p50, p95, max };
}

export function loadShape(environment: Record<string, string | undefined>) {
  const count = Number(environment.ADR71_LOAD_REQUESTS ?? 48);
  const concurrency = Number(environment.ADR71_LOAD_CONCURRENCY ?? 12);
  const rounds = Number(environment.ADR71_LOAD_ROUNDS ?? 1);
  assert.ok(Number.isInteger(count) && Number.isInteger(concurrency)
    && Number.isInteger(rounds) && count > 0 && concurrency > 0
    && concurrency <= count && rounds > 0 && rounds <= 100,
    `invalid load shape requests=${count} concurrency=${concurrency} rounds=${rounds}`);
  return { count, concurrency, rounds };
}

async function load(base: string) {
  // Cause/effect graph: C1 each new Resource-bearing K8s Session owns a distinct
  // Environment Work lane and has no reusable Pod because its projected mounts
  // are Session-specific -> first execution includes bounded Pod/material cold
  // start; C2 every Session has completed one cold start -> its live environment
  // is resident; C3 bounded concurrent writes below admission limits -> zero HTTP
  // errors; C4 active/active Coordinator and Worker dispatch -> unique Run ids and
  // exactly-once outputs. A shared Environment belongs to the separate one-active
  // WorkQueue safety contract and is rejected by the fleet planner. Effects are E1 a
  // separately bounded cold-start result and E2 the steady-state completed-turn
  // p95. Every latency starts before one SDK send and ends only when that exact
  // receipt's Provider marker and later idle are durable. The admission response
  // alone is never treated as synchronous turn completion. Mixing C1 into E2
  // would make the steady-state contract depend on Kubelet image/sidecar startup.
  //
  // | Rule | Resource Pod resident | Concurrent load | Effect |
  // |---|---|---|---|
  // | L2 | no + public reads not responsive | read-only public probes |
  // |    | establish post-failover recovery before the cold timer starts |
  // | L1 | no + public reads responsive | one turn per Session completes |
  // |    | within the unchanged cold-start bound |
  // | L3 | yes + public reads responsive | bounded receipt-scoped turns | zero
  // |    | errors and marker+idle completion p95 <= configured limit |
  // | L4 | yes | bounded + active/active | every full-history marker exactly once |
  const {count,concurrency,rounds} = loadShape(process.env);
  // Five seconds is a performance regression gate for the complete
  // public->Coordinator->claim->Worker->Provider->commit->idle round trip, not
  // an admission SLO.
  const p95Limit = Number(process.env.ADR71_TURN_P95_MS ?? 5_000);
  const coldStartLimit = Number(process.env.ADR71_COLD_START_MAX_MS ?? 30_000);
  const managed = clientFor(base);
  const sessions = await createFleetSessions(managed, concurrency);
  // L2 must precede L1's timer after a database promotion: these GETs neither
  // materialize a Resource Pod nor execute a turn, but establish that the
  // public read path has recovered before measuring fresh Pod realization.
  await awaitResponsivePublicReads(managed, sessions);
  const coldMarkers = sessions.map((_, index) => `ADR71-COLD-${Date.now()}-${index}`);
  const coldObservations = await Promise.all(sessions.map((sessionId, index) =>
    sendAndWaitForAgentMarker(managed, sessionId, coldMarkers[index], coldStartLimit)));
  const coldElapsed = Math.max(...coldObservations.map((observation) => observation.elapsed));
  console.log(`COLD AUDIT ${JSON.stringify(coldObservations.map((observation, index) => ({
    session: index,
    elapsedMs: observation.elapsed,
    admissionMs: observation.admissionMs,
    observationMs: observation.observationMs,
  })))}`);
  assert.ok(
    coldElapsed <= coldStartLimit,
    `parallel Resource Pod cold start ${coldElapsed.toFixed(1)}ms > ${coldStartLimit}ms`,
  );
  const histories = coldObservations.map((observation) => observation.events);
  coldObservations.forEach((observation, index) => {
    assertProviderResponseCount(observation.events, coldMarkers[index]);
    assertProviderResponsesAreUnique(observation.events);
  });
  // The completed cold turns leave these exact Resource Pods resident for L3.
  // L6 repeats use these same resident Sessions and the same per-window gate.
  // No averaging or best-of selection can hide a failed window; an assertion
  // stops later writes, retaining every prior audit and the failed evidence.
  for (let round = 1; round <= rounds; round += 1) {
    const markers = Array.from({length:count}, (_,index) => `ADR71-LOAD-${Date.now()}-${round}-${index}`);
    const latencies: number[] = [];
    const samples: { marker: string; admissionMs: number; observationMs: number }[] = [];
    async function runClient(clientIndex: number) {
      for (let index = clientIndex; index < markers.length; index += concurrency) {
        const started = performance.now();
        const receipt = await managed.beta.sessions.events.send(sessions[clientIndex], {
          events: [{ type: 'user.message', content: [{ type: 'text', text: markers[index] }] }],
          betas: BETAS,
        });
        const acceptedAt = performance.now();
        assert.equal(receipt.data?.length, 1, `one exact Event receipt for ${markers[index]}`);
        const receiptId = receipt.data?.[0]?.id;
        assert.equal(typeof receiptId, 'string', `SDK receipt id for ${markers[index]}`);
        await waitForRecentSessionEventReceipt(
          managed, sessions[clientIndex], receiptId, markers[index], 300_000,
        );
        const observedAt = performance.now();
        const elapsed = observedAt - started;
        latencies.push(elapsed);
        samples.push({ marker: markers[index], admissionMs: acceptedAt - started, observationMs: observedAt - acceptedAt });
      }
    }
    // A failed Session stops its own dependent sends. Join the independent
    // clients before auditing; do not replay an ambiguously accepted write.
    const clients = await Promise.allSettled(Array.from({ length: concurrency }, (_, index) => runClient(index)));
    const markersBySession = sessions.map(() => [] as string[]);
    markers.forEach((marker, index) => markersBySession[index % concurrency].push(marker));
    // Refresh durable history after all clients settle, including those whose
    // response was lost. Cached successful observations cannot establish the
    // outcome of the last failed request. A failed read remains an audit error.
    const finalReads = await Promise.allSettled(
      sessions.map((session) => listManagedSessionEvents(managed, session, BETAS)),
    );
    finalReads.forEach((read, index) => {
      if (read.status === 'fulfilled') histories[index] = read.value;
    });
    const audit = auditLoadResults(histories, markersBySession, latencies, p95Limit);
    finalReads.forEach((read, index) => {
      if (read.status === 'rejected') audit.errors.push(`Session ${index} final history: ${String(read.reason)}`);
    });
    clients.forEach((client, index) => {
      if (client.status === 'rejected') audit.errors.push(`Session ${index}: ${String(client.reason)}`);
    });
    console.log(`LOAD AUDIT ${JSON.stringify({ ...audit, round, rounds, coldMs: coldElapsed, samples })}`);
    assert.deepEqual(audit.errors, [], 'L3-L5 complete latency and durable-history audit');
    console.log(
      `OK load round=${round}/${rounds} requests=${count} concurrency=${concurrency} cold_ms=${coldElapsed.toFixed(1)} `
        + `turn_p50_ms=${audit.p50} turn_p95_ms=${audit.p95} turn_max_ms=${audit.max}`,
    );
  }
}

async function main() {
  const [mode, base, ...args] = process.argv.slice(2);
  switch (mode) {
    case 'bootstrap': return bootstrap(base);
    case 'unavailable-publication': return unavailablePublication(base);
    case 'retry-publication': return retryPublication(base);
    case 'verify-durable': return verifyDurable(base, args[0], args[1], args[2]);
    case 'read-durable': return readDurableDuringFault(base, args[0], args[1], args[2]);
    case 'prepare-native-crash': return prepareNativeCrash(base, args[0], args[1]);
    case 'verify-native-crash': return verifyNativeCrash(base, args[0], args[1], args[2]);
    case 'observe-provider-inflight': return observeNativeProvider(base, args[0], false);
    case 'observe-provider-completed': return observeNativeProvider(base, args[0], true);
    case 'release-provider-inflight': return releaseNativeProvider(base, args[0]);
    case 'verify-native-inflight': return verifyNativeInFlight(base, args[0], args[1], args[2]);
    case 'batch': return batch(base, args[0], args[1], args[2], Number(args[3]));
    case 'dispose-batch': return disposeBatch(base, args[0]);
    case 'load': return load(base);
    default: throw new Error(`unknown mode ${mode}`);
  }
}

if (process.argv[1] && fileURLToPath(import.meta.url) === resolve(process.argv[1])) {
  main().catch((error) => {
    console.error(error);
    process.exitCode = 1;
  });
}
