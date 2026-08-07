// Black-box ADR-0071 driver. Every operation uses the one public Ingress URL;
// private Control/Coordinator/Worker addresses are intentionally unknowable here.
import assert from 'node:assert/strict';

const BETAS = 'managed-agents-2026-04-01';
const WORKSPACE = 'workspace-adr71';
const AGENT = 'adr71-agent';

function betaFor(route: string) {
  if (route.startsWith('/v1/memory_stores')) return 'agent-memory-2026-07-22';
  if (route.startsWith('/v1/skills')) return 'skills-2025-10-02';
  return BETAS;
}

async function api(base: string, method: string, route: string, body?: unknown) {
  const response = await fetch(`${base}${route}`, {
    method,
    headers: {
      'anthropic-beta': betaFor(route),
      ...(body === undefined ? {} : { 'content-type': 'application/json' }),
    },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  const text = await response.text();
  let value: any = null;
  try { value = text ? JSON.parse(text) : null; } catch { value = text; }
  return { status: response.status, value, text };
}

async function expectStatus(
  base: string,
  method: string,
  route: string,
  status: number,
  body?: unknown,
) {
  const result = await api(base, method, route, body);
  assert.equal(result.status, status, `${method} ${route}: ${JSON.stringify(result.value)}`);
  return result.value;
}

async function expectIdempotentStatusEventually(
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
  let result = await api(base, method, route, body);
  while (Date.now() < deadline && [502, 503, 504].includes(result.status)) {
    await new Promise((resolve) => setTimeout(resolve, 200));
    result = await api(base, method, route, body);
  }
  assert.equal(result.status, status, `${method} ${route}: ${JSON.stringify(result.value)}`);
  return result.value;
}

async function expectStableStatus(
  base: string,
  method: string,
  route: string,
  status: number,
  consecutive = 5,
  timeoutMs = 20_000,
) {
  assert.equal(method, 'GET', `stability probe refuses a business command: ${method} ${route}`);
  const deadline = Date.now() + timeoutMs;
  let stable = 0;
  let result = await api(base, method, route);
  while (Date.now() < deadline) {
    stable = result.status === status ? stable + 1 : 0;
    if (stable >= consecutive) return result.value;
    await new Promise((resolve) => setTimeout(resolve, 200));
    result = await api(base, method, route);
  }
  assert.equal(result.status, status, `${method} ${route}: ${JSON.stringify(result.value)}`);
  assert.fail(`${method} ${route} did not remain at ${status} for ${consecutive} probes`);
}

async function awaitResponsiveCoordinatorSet(
  base: string,
  sessionIds: string[],
  timeoutMs = 30_000,
  maxReadMs = 2_000,
) {
  const deadline = Date.now() + timeoutMs;
  let last = 'no probe completed';
  while (Date.now() < deadline) {
    // Use two reads per Session so the public edge opens enough upstream
    // connections to exercise both replicated Coordinator pools without naming
    // or calling a private Pod. Reads are repeatable and carry no business effect.
    const probes = sessionIds.flatMap((sessionId) => [sessionId, sessionId]);
    const results = await Promise.all(probes.map(async (sessionId) => {
      const started = performance.now();
      const response = await api(base, 'GET', `/v1/sessions/${sessionId}`);
      return { ...response, elapsed: performance.now() - started };
    }));
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
  const response = await fetch(`${base}/v1/files`, {
    method: 'POST', headers: { 'anthropic-beta': BETAS }, body: form,
  });
  const text = await response.text();
  assert.equal(response.status, 200, `POST /v1/files: ${text}`);
  return JSON.parse(text);
}

async function events(base: string, sessionId: string) {
  return api(base, 'GET', `/v1/sessions/${sessionId}/events`);
}

async function waitForAgentMarkers(
  base: string,
  sessionId: string,
  markers: string[],
  timeoutMs = 180_000,
) {
  const deadline = Date.now() + timeoutMs;
  let rendered = '';
  while (Date.now() < deadline) {
    const result = await events(base, sessionId);
    if (result.status === 200) {
      rendered = JSON.stringify(result.value);
      const counts = providerResponseCounts(rendered);
      if (markers.every((marker) => counts.has(`ADR71-PROVIDER:${marker}`))) return rendered;
    }
    await new Promise((resolve) => setTimeout(resolve, 500));
  }
  throw new Error(`session ${sessionId} did not commit all Provider markers: ${markers}\n${rendered}`);
}

function providerResponseCounts(rendered: string) {
  const body = JSON.parse(rendered);
  const responses = (body.data ?? [])
    .filter((event: any) => event.type === 'agent.message')
    .flatMap((event: any) => event.content ?? [])
    .filter((block: any) => block.type === 'text' && block.text.startsWith('ADR71-PROVIDER:'))
    .map((block: any) => block.text);
  const counts = new Map<string, number>();
  for (const response of responses) counts.set(response, (counts.get(response) ?? 0) + 1);
  return counts;
}

function assertProviderResponseCount(rendered: string, marker: string) {
  assert.equal(
    providerResponseCounts(rendered).get(`ADR71-PROVIDER:${marker}`),
    1,
    `expected one exact Provider response for ${marker}`,
  );
}

function assertProviderResponsesAreUnique(rendered: string) {
  const counts = providerResponseCounts(rendered);
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
  // candidate -> one immutable snapshot; B5 Deployment launch -> one Session;
  // B6 real Worker materializes all pins,
  // calls the authenticated Provider, and commits one claim-fenced response.
  await expectStatus(base, 'POST', '/internal/v1/executable-agents/withdraw', 404, {
    workspace_id: 'auth-probe', agent_id: 'auth-probe', lifecycle_revision: 1,
  });
  await expectStatus(base, 'GET', '/v1/config/catalog', 200);
  await expectStatus(base, 'GET', '/v1/models', 200);

  const credentialSecret = 'adr71-must-never-be-returned'; // awaken-allow: secret
  const deniedCredential = await expectStatus(
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
  await expectStatus(base, 'GET', `/v1/files/${file.id}/content`, 400);

  const memory = await expectStatus(base, 'POST', '/v1/memory_stores', 200, {
    name: 'adr71-memory', description: 'distributed Worker memory proof',
  });
  await expectStatus(base, 'GET', `/v1/memory_stores/${memory.id}/config`, 404);
  await expectStatus(base, 'POST', `/v1/memory_stores/${memory.id}/memories`, 200, {
    path: '/fact.md', content: 'ADR71-MEMORY-MATERIALIZED',
  });

  const skill = await expectStatus(base, 'POST', '/v1/skills', 200, {
    id: 'adr71-skill',
    content: '---\nname: adr71-skill\ndescription: distributed Worker skill proof\nenvironment: filesystem\n---\nADR71-SKILL-MATERIALIZED',
  });

  const agentId = 'adr71-agent';
  await expectStatus(base, 'PUT', `/v1/config/agents/${agentId}`, 200, {
    name: 'ADR-0071 distributed Agent',
    system: 'Answer the user through the configured provider.',
    model: { id: 'adr71-echo' }, tools: [],
    skills: [{ type: 'custom', skill_id: skill.id, version: '1' }],
  });
  await expectStatus(base, 'PUT', `/v1/config/agents/${agentId}/resources`, 200, {
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
  const publication = await expectStatus(base, 'POST', `/v1/config/agents/${agentId}/publish`, 200);
  assert.ok(publication.fingerprint, JSON.stringify(publication));

  const environment = await expectStatus(base, 'POST', '/v1/environments', 200, {
    name: 'adr71-distributed', config: { type: 'cloud' },
  });
  const initialMarker = 'ADR71-INITIAL-RESPONSE';
  const deployment = await expectStatus(base, 'POST', '/v1/deployments', 200, {
    agent: agentId, environment_id: environment.id, name: 'adr71-deployment',
    initial_events: [{
      type: 'user.message', content: [{ type: 'text', text: initialMarker }],
    }],
  });
  const run = await expectStatus(base, 'POST', `/v1/deployments/${deployment.id}/run`, 200);
  assert.equal(run.error, null, JSON.stringify(run));
  assert.ok(run.session_id, JSON.stringify(run));
  await waitForAgentMarkers(base, run.session_id, [initialMarker]);
  const restoredMemory = await expectStatus(base, 'GET', `/v1/memory_stores/${memory.id}`, 200);
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
  await expectStatus(base, 'PUT', `/v1/config/agents/${agentId}`, 200, {
    name: 'ADR-0071 recovery Agent', system: 'Recover registration.',
    model: { id: 'adr71-echo' }, tools: [],
  });
  const unavailable = await api(base, 'POST', `/v1/config/agents/${agentId}/publish`);
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
  const publication = await expectIdempotentStatusEventually(
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
  // stable, then issue the non-idempotent Event command once; an ambiguous write
  // response is never blindly replayed because this route has no idempotency key;
  // R3 accepted Event -> a live Worker commits exactly one Provider response;
  // R4 every prior Provider marker is also unique, so an earlier ambiguous write
  // cannot be hidden by a later successful read.
  const deployment = await expectIdempotentStatusEventually(
    base, 'GET', `/v1/deployments/${deploymentId}`, 200, undefined, 60_000,
  );
  assert.equal(deployment.id, deploymentId, JSON.stringify(deployment));
  const session = await expectStableStatus(
    base, 'GET', `/v1/sessions/${sessionId}`, 200, 5, 60_000,
  );
  assert.equal(session.id, sessionId, JSON.stringify(session));
  const receipts = await expectStatus(base, 'POST', `/v1/sessions/${sessionId}/events`, 200, {
    events: [{ type: 'user.message', content: [{ type: 'text', text: marker }] }],
  });
  assert.equal(receipts.data?.length, 1, JSON.stringify(receipts));
  const rendered = await waitForAgentMarkers(base, sessionId, [marker]);
  assertProviderResponseCount(rendered, marker);
  assertProviderResponsesAreUnique(rendered);
  console.log(`OK durable flow ${marker}`);
}

async function createParallelSessions(base: string, environmentId: string, count: number) {
  return Promise.all(Array.from({ length: count }, async () => {
    const session = await expectStatus(base, 'POST', '/v1/sessions', 200, {
      agent: AGENT,
      environment_id: environmentId,
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
  // both Workers carry real parallel claims and every accepted event terminates;
  // C2 claim fencing -> no duplicate Provider response after recovery/reclaim;
  // C3 the original durable Session remains one member of the same batch.
  const markers = Array.from({ length: count }, (_, index) => `${prefix}-${index}`);
  const sessions = [
    originalSessionId,
    ...await createParallelSessions(base, environmentId, Math.max(0, count - 1)),
  ];
  const responses = await Promise.all(markers.map((marker, index) => api(
    base, 'POST', `/v1/sessions/${sessions[index]}/events`,
    { events: [{ type: 'user.message', content: [{ type: 'text', text: marker }] }] },
  )));
  responses.forEach((response, index) => {
    assert.equal(response.status, 200, `batch ${markers[index]}: ${response.text}`);
  });
  for (let index = 0; index < markers.length; index += 1) {
    const marker = markers[index];
    const rendered = await waitForAgentMarkers(base, sessions[index], [marker], 240_000);
    assertProviderResponseCount(rendered, marker);
    assertProviderResponsesAreUnique(rendered);
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
  // separately bounded cold-start result and E2 the steady-state synchronous-turn
  // p95. POST /events intentionally returns after Runtime execution and settlement;
  // it is not a queue-admission acknowledgement. Mixing C1 into E2 made the
  // steady-state contract depend on Kubelet image/sidecar startup, while calling
  // E2 an acknowledgement invented a second API semantic that production does not
  // implement.
  //
  // | Rule | Resource Pod resident | Concurrent load | Effect |
  // |---|---|---|---|
  // | L1 | no | one per Session | complete once within cold-start bound |
  // | L2 | yes + DB failover pools not converged | read-only public probes |
  // |    | expose recovery latency; never start the steady-state timer |
  // | L3 | yes + every replica responsive | bounded synchronous turns | zero
  // |    | errors and steady completion p95 <= configured limit |
  // | L4 | yes | bounded + active/active | every marker exactly once |
  const count = Number(process.env.ADR71_LOAD_REQUESTS ?? 48);
  const concurrency = Number(process.env.ADR71_LOAD_CONCURRENCY ?? 12);
  // Five seconds is a performance regression gate for the complete
  // public->Coordinator->claim->Worker->Provider->commit round trip, not an
  // admission SLO. The edge's separate 70-second correctness timeout remains the
  // outer failure boundary for a non-idempotent command.
  const p95Limit = Number(process.env.ADR71_TURN_P95_MS ?? 5_000);
  const coldStartLimit = Number(process.env.ADR71_COLD_START_MAX_MS ?? 30_000);
  const markers = Array.from({ length: count }, (_, index) => `ADR71-LOAD-${Date.now()}-${index}`);
  const sessions = await createParallelSessions(base, environmentId, concurrency);
  const coldStartedAt = performance.now();
  const coldMarkers = sessions.map((_, index) => `ADR71-COLD-${Date.now()}-${index}`);
  const coldResponses = await Promise.all(sessions.map((sessionId, index) => api(
    base, 'POST', `/v1/sessions/${sessionId}/events`,
    { events: [{ type: 'user.message', content: [{ type: 'text', text: coldMarkers[index] }] }] },
  )));
  coldResponses.forEach((response, index) => {
    assert.equal(response.status, 200, `cold start ${coldMarkers[index]}: ${response.text}`);
  });
  const coldElapsed = performance.now() - coldStartedAt;
  assert.ok(
    coldElapsed <= coldStartLimit,
    `parallel Resource Pod cold start ${coldElapsed.toFixed(1)}ms > ${coldStartLimit}ms`,
  );
  await Promise.all(sessions.map((sessionId, index) =>
    waitForAgentMarkers(base, sessionId, [coldMarkers[index]], coldStartLimit)));
  // The promoted standby preserves accepted facts, but each replicated SQLx pool
  // discovers its old idle socket independently. Converge them with repeatable
  // public reads before measuring the separate steady-state SLO; using one
  // successful write here would warm only its randomly selected Coordinator.
  await awaitResponsiveCoordinatorSet(base, sessions);
  const latencies: number[] = [];
  async function client(clientIndex: number) {
    for (let index = clientIndex; index < markers.length; index += concurrency) {
      const started = performance.now();
      const result = await api(base, 'POST', `/v1/sessions/${sessions[clientIndex]}/events`, {
        events: [{ type: 'user.message', content: [{ type: 'text', text: markers[index] }] }],
      });
      latencies.push(performance.now() - started);
      assert.equal(result.status, 200, `load ${index}: ${result.text}`);
    }
  }
  await Promise.all(Array.from({ length: concurrency }, (_, index) => client(index)));
  latencies.sort((left, right) => left - right);
  const p50 = latencies[Math.min(latencies.length - 1, Math.ceil(latencies.length * 0.50) - 1)];
  const p95 = latencies[Math.min(latencies.length - 1, Math.ceil(latencies.length * 0.95) - 1)];
  const max = latencies[latencies.length - 1];
  assert.ok(
    p95 <= p95Limit,
    `synchronous turn p95 ${p95.toFixed(1)}ms > ${p95Limit}ms `
      + `(p50=${p50.toFixed(1)}ms max=${max.toFixed(1)}ms)`,
  );
  const markersBySession = sessions.map(() => [] as string[]);
  markers.forEach((marker, index) => markersBySession[index % concurrency].push(marker));
  for (let index = 0; index < sessions.length; index += 1) {
    const rendered = await waitForAgentMarkers(
      base, sessions[index], markersBySession[index], 300_000,
    );
    for (const marker of markersBySession[index]) assertProviderResponseCount(rendered, marker);
    assertProviderResponsesAreUnique(rendered);
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
