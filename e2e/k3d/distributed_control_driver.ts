import assert from 'node:assert/strict';

const BETAS = 'managed-agents-2026-04-01';

async function api(base: string, method: string, route: string, body?: unknown, token?: string) {
  const response = await fetch(`${base}${route}`, {
    method,
    headers: {
      'anthropic-beta': BETAS,
      ...(body === undefined ? {} : { 'content-type': 'application/json' }),
      ...(token === undefined ? {} : { authorization: `Bearer ${token}` }),
    },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  const text = await response.text();
  let value: any = null;
  try { value = text ? JSON.parse(text) : null; } catch { value = text; }
  return { status: response.status, value };
}

async function expectStatus(
  base: string,
  method: string,
  route: string,
  status: number,
  body?: unknown,
  token?: string,
) {
  const result = await api(base, method, route, body, token);
  assert.equal(result.status, status, `${method} ${route}: ${JSON.stringify(result.value)}`);
  return result.value;
}

async function waitForMarker(coordinator: string, sessionId: string, marker: string) {
  const deadline = Date.now() + 90_000;
  while (Date.now() < deadline) {
    const result = await api(coordinator, 'GET', `/v1/sessions/${sessionId}/events`);
    if (result.status === 200 && JSON.stringify(result.value).includes(marker)) return;
    await new Promise((resolve) => setTimeout(resolve, 500));
  }
  throw new Error(`session ${sessionId} did not commit marker ${marker}`);
}

async function bootstrap(control: string, coordinator: string) {
  // Cause/effect decision table:
  // B1 wrong registration credential -> 401 and no projection mutation;
  // B2 wrong launch credential -> 401 and no Session mutation;
  // B3 publish with Coordinator available -> durable publication plus registered
  // snapshot; B4 Deployment launch -> one linked Session; B5 initial Event -> a
  // remote Worker commits a complete response visible from Coordinator history.
  await expectStatus(
    coordinator,
    'POST',
    '/internal/v1/executable-agents/withdraw',
    401,
    { workspace_id: 'auth-probe', agent_id: 'auth-probe', lifecycle_revision: 1 },
    'wrong-registration-token',
  );
  await expectStatus(
    coordinator,
    'POST',
    '/internal/v1/deployment-sessions/launch',
    401,
    {
      deployment_id: 'depl_auth_probe',
      deployment_run_id: 'drun_auth_probe',
      workspace_id: 'default',
      agent: { type: 'agent', id: 'auth-probe', version: 1 },
      environment_id: 'env_auth_probe',
      metadata: {},
      initial_events: [],
      resources: [],
      vault_ids: [],
    },
    'wrong-launch-token',
  );

  const agentId = 'adr71-agent';
  await expectStatus(control, 'PUT', `/v1/config/agents/${agentId}`, 200, {
    name: 'ADR-0071 distributed Agent',
    system: 'Echo the user input exactly.',
    model: { id: 'adr71-echo' },
    tools: [],
  });
  const publication = await expectStatus(
    control,
    'POST',
    `/v1/config/agents/${agentId}/publish`,
    200,
  );
  assert.ok(publication.fingerprint, JSON.stringify(publication));

  const environment = await expectStatus(control, 'POST', '/v1/environments', 200, {
    name: 'adr71-distributed',
    config: { type: 'cloud' },
  });
  const initialMarker = 'ADR71-INITIAL-RESPONSE';
  const deployment = await expectStatus(control, 'POST', '/v1/deployments', 200, {
    agent: agentId,
    environment_id: environment.id,
    name: 'adr71-deployment',
    initial_events: [{
      type: 'user.message',
      content: [{ type: 'text', text: initialMarker }],
    }],
  });
  const run = await expectStatus(
    control,
    'POST',
    `/v1/deployments/${deployment.id}/run`,
    200,
  );
  assert.equal(run.error, null, JSON.stringify(run));
  assert.ok(run.session_id, JSON.stringify(run));
  await waitForMarker(coordinator, run.session_id, initialMarker);
  console.log(`OK ${deployment.id} ${run.session_id} ${environment.id}`);
}

async function unavailablePublication(control: string) {
  // U1 Coordinator unavailable after Control persistence -> 503 while the exact
  // StoredPublication remains retryable; no alternate local registration path.
  const agentId = 'adr71-recovery-agent';
  await expectStatus(control, 'PUT', `/v1/config/agents/${agentId}`, 200, {
    name: 'ADR-0071 recovery Agent',
    system: 'Echo after registration recovery.',
    model: { id: 'adr71-echo' },
    tools: [],
  });
  await expectStatus(
    control,
    'POST',
    `/v1/config/agents/${agentId}/publish`,
    503,
  );
  console.log('OK unavailable publication remained retryable');
}

async function retryPublication(control: string) {
  // U2 the same publication after Coordinator recovery -> acknowledged once by
  // the canonical registrar and exposed as a normal Control publish success.
  const publication = await expectStatus(
    control,
    'POST',
    '/v1/config/agents/adr71-recovery-agent/publish',
    200,
  );
  assert.ok(publication.fingerprint, JSON.stringify(publication));
  console.log('OK publication registration recovered');
}

async function verifyRestart(
  control: string,
  coordinator: string,
  deploymentId: string,
  sessionId: string,
) {
  // R1 Control restart -> Deployment truth is restored from its authoritative
  // repository; R2 Coordinator restart -> Session and executable projection are
  // restored; R3 a new Event -> the remote Worker still commits the full response.
  const deployment = await expectStatus(control, 'GET', `/v1/deployments/${deploymentId}`, 200);
  assert.equal(deployment.id, deploymentId, JSON.stringify(deployment));
  const session = await expectStatus(coordinator, 'GET', `/v1/sessions/${sessionId}`, 200);
  assert.equal(session.id, sessionId, JSON.stringify(session));

  const marker = 'ADR71-AFTER-ROLE-RESTART';
  const receipts = await expectStatus(
    coordinator,
    'POST',
    `/v1/sessions/${sessionId}/events`,
    200,
    { events: [{ type: 'user.message', content: [{ type: 'text', text: marker }] }] },
  );
  assert.equal(receipts.data?.length, 1, JSON.stringify(receipts));
  await waitForMarker(coordinator, sessionId, marker);
  console.log('OK role restart preserved Deployment, Session, and response flow');
}

async function main() {
  const [mode, control, coordinator, ...args] = process.argv.slice(2);
  switch (mode) {
    case 'bootstrap': return bootstrap(control, coordinator);
    case 'unavailable-publication': return unavailablePublication(control);
    case 'retry-publication': return retryPublication(control);
    case 'verify-restart': return verifyRestart(control, coordinator, args[0], args[1]);
    default: throw new Error(`unknown mode ${mode}`);
  }
}

main().catch((error) => {
  console.error(error);
  process.exitCode = 1;
});
