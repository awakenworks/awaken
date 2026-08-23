// Real-process E2E for the complete Postgres control plane.
//
// Catalog, credential, Agent publication, admin resources/webhooks, and Session
// state are independent repository ports. This scenario selects their Postgres
// adapters together, drives only public HTTP, restarts the process, and proves the
// same immutable publication still executes without a node-local config truth.

import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { execFileSync, type ChildProcess } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import Anthropic from '@anthropic-ai/sdk';
import type { BetaManagedAgentsSessionEvent } from '@anthropic-ai/sdk/resources/beta/sessions/events';
import { startFakeAnthropic } from './fixtures/fake_anthropic_fixture.mjs';
// @ts-ignore -- shared JS harness deliberately serves both JS and TS scenarios.
import { spawnProduction, stopServer, waitForPort, waitForSessionEventReceipt } from './harness.mjs';

const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const PORT = Number(process.env.E2E_PORT ?? 39413);
const WORKSPACE = `workspace_control_pg_${process.pid}`;
const AGENT = `control-pg-agent-${process.pid}`;
const MODEL = 'fake-haiku';
const PROVIDER = 'anthropic';
const FAKE_KEY = `sk-control-pg-${process.pid}`; // awaken-allow: secret
const SEAL_KEY = '00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff';
const BETAS = ['managed-agents-2026-04-01'];
const sleep = (milliseconds: number) =>
  new Promise((resolve) => setTimeout(resolve, milliseconds));

function docker(...args: string[]): string {
  return execFileSync('docker', args, { cwd: ROOT, encoding: 'utf8' }).trim();
}

async function postgres(): Promise<{ container: string; url: string; owned: boolean }> {
  const inheritedUrl = process.env.SESSION_DEPLOYMENT_DATABASE_URL;
  const inheritedContainer = process.env.AWAKEN_E2E_POSTGRES_CONTAINER;
  if (inheritedUrl && inheritedContainer) {
    return { container: inheritedContainer, url: inheritedUrl, owned: false };
  }
  const container = `awaken-control-plane-pg-${process.pid}`;
  docker(
    'run', '-d', '--name', container,
    '-e', 'POSTGRES_PASSWORD=test',
    '-e', 'POSTGRES_DB=awaken',
    '-p', '127.0.0.1::5432',
    '--health-cmd=pg_isready -U postgres -d awaken',
    '--health-interval=1s', '--health-timeout=2s', '--health-retries=30',
    'postgres:16-alpine',
  );
  const deadline = Date.now() + 60_000;
  while (Date.now() < deadline) {
    const health = docker('inspect', '--format', '{{.State.Health.Status}}', container);
    if (health === 'healthy') {
      const mapping = docker('port', container, '5432/tcp').split('\n')[0];
      return {
        container,
        url: `postgres://postgres:test@127.0.0.1:${mapping.slice(mapping.lastIndexOf(':') + 1)}/awaken`,
        owned: true,
      };
    }
    await sleep(250);
  }
  throw new Error('timed out waiting for disposable Postgres');
}

function start(directory: string, databaseUrl: string): ChildProcess {
  // Authentication is an independent axis in this persistence scenario.
  // no-login makes requests exercise only the five selected Postgres
  // repositories; the canonical production helper also disables unrelated ACP
  // discovery so provider probing cannot consume this persistence gate's boot
  // deadline.
  return spawnProduction(directory, PORT, {
    workspace: WORKSPACE,
    identityMode: 'no-login',
    controlSealKey: SEAL_KEY,
    databases: {
      catalog_db: databaseUrl,
      credential_db: databaseUrl,
      config_db: databaseUrl,
      admin_db: databaseUrl,
      sessions_db: databaseUrl,
    },
    stderr: 'inherit',
  });
}

async function request(method: string, uri: string, body?: unknown) {
  const response = await fetch(`http://127.0.0.1:${PORT}${uri}`, {
    method,
    headers: body === undefined ? {} : { 'content-type': 'application/json' },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  const text = await response.text();
  let parsed: any = null;
  try {
    parsed = text ? JSON.parse(text) : null;
  } catch {
    parsed = text;
  }
  return { status: response.status, body: parsed };
}

async function executePublishedAgent(expectedCalls: number): Promise<void> {
  const client = new Anthropic({
    apiKey: 'e2e-dummy',
    baseURL: `http://127.0.0.1:${PORT}`,
  });
  const session = await client.beta.sessions.create({
    agent: AGENT,
    environment_id: 'env_local',
    betas: BETAS,
  });
  // Durable-admission cause/effect table: R1 exact User Event receipt is still
  // unprocessed => keep observing; R2 it is processed but its later Agent
  // message is absent => keep observing; R3 only a later matching message may
  // prove this Run. A one-shot list can observe admission before execution and
  // must not turn that legal state into a persistence failure. Causes are the
  // exact receipt and PostgreSQL-backed publication; effects are processed
  // receipt plus its later matching Agent Message. Constraints/invariant: no
  // pre-existing history or unrelated Run may satisfy expectedCalls.
  const receipt = await client.beta.sessions.events.send(session.id, {
    events: [{ type: 'user.message', content: [{ type: 'text', text: `postgres-${expectedCalls}` }] }],
    betas: BETAS,
  });
  const receiptId = receipt.data?.[0]?.id;
  assert.equal(typeof receiptId, 'string', 'official SDK returns the exact User Event receipt');
  const observed: { delta: BetaManagedAgentsSessionEvent[] } =
    await waitForSessionEventReceipt(
      client,
      session.id,
      receiptId,
      BETAS,
      ({ delta }: { delta: BetaManagedAgentsSessionEvent[] }) =>
        delta.some((event) => event.type === 'agent.message'
          && event.content.some((part) => part.type === 'text'
            && part.text.includes(`FAKE:postgres-${expectedCalls}`))),
      `PostgreSQL-backed publication Run ${expectedCalls} to commit its Agent message`,
    );
  const message = observed.delta.find((event) => event.type === 'agent.message');
  assert.ok(message?.type === 'agent.message');
  const text = message.content
    .filter((part) => part.type === 'text')
    .map((part) => part.text)
    .join('');
  assert.match(text, new RegExp(`FAKE:postgres-${expectedCalls}`));
}

async function main(): Promise<void> {
  const database = await postgres();
  const upstream = await startFakeAnthropic(FAKE_KEY, { models: [MODEL] });
  const directory = fs.mkdtempSync(path.join(os.tmpdir(), 'awaken-control-pg-'));
  let server = start(directory, database.url);

  try {
    // Startup cause/effect graph: C1 all five Postgres repositories reachable;
    // C2 production child remains live; C3 listener appears before the bounded
    // monotonic deadline. Effects: E1 proceed only for C1+C2+C3; E2 a terminal
    // child reports its exact exit immediately; E3 a live-but-stalled child
    // fails at 120s. Decision table: R1 C1+C2+C3 -> drive persistence; R2 !C2
    // -> E2; R3 C2+!C3 -> E3. The same rules are applied after replacement.
    await waitForPort(PORT, 120_000, server);

    let response = await request('POST', '/v1/config/provider-connections', {
      idempotency_key: 'control-postgres-provider-connection',
      workspace_id: WORKSPACE,
      provider_id: PROVIDER,
      display_name: 'Postgres provider',
      dialect: 'anthropic_messages',
      base_url: `${upstream.url}/v1/`,
      timeout_secs: 30,
      secret: FAKE_KEY,
    });
    assert.equal(response.status, 201, JSON.stringify(response.body));
    const credentialId = response.body.credential.id;

    response = await request('PUT', `/v1/config/model-attributes/${MODEL}`, {
      context_window: 8192,
      max_output_tokens: 1024,
    });
    assert.equal(response.status, 200, JSON.stringify(response.body));

    const rejectedCredentials = [
      {
        kind: 'env',
        provider_id: PROVIDER,
        env_key: 'ANTHROPIC_API_KEY',
      },
      {
        kind: 'vault',
        provider_id: PROVIDER,
      },
      {
        kind: 'oauth',
        provider_id: PROVIDER,
        oauth_helper: 'gcloud',
        secret: FAKE_KEY,
      },
      {
        kind: 'worker_local',
        provider_id: PROVIDER,
        secret: FAKE_KEY,
      },
      {
        kind: 'worker_local',
        provider_id: PROVIDER,
        env_key: 'WORKER_PRIVATE_KEY',
      },
    ];
    for (const invalid of rejectedCredentials) {
      response = await request('POST', '/v1/config/credentials', {
        workspace_id: WORKSPACE,
        ...invalid,
      });
      assert.equal(response.status, 422, JSON.stringify(response.body));
      assert.equal(response.body.code, 'credential_invalid');
    }

    response = await request('PUT', `/v1/config/inference-profiles/profile-${process.pid}`, {
      primary: {
        target: { model_id: MODEL },
        credential_binding: { type: 'exact', credential_source_id: credentialId },
      },
      disabled_endpoint_ids: [],
    });
    assert.equal(response.status, 200, JSON.stringify(response.body));
    response = await request('GET', `/v1/config/inference-profiles/profile-${process.pid}`);
    assert.equal(response.status, 200, JSON.stringify(response.body));

    response = await request('PUT', `/v1/config/agents/${AGENT}`, {
      name: AGENT,
      model: { id: MODEL },
      system: 'Postgres control-plane E2E',
      max_steps: 2,
    });
    assert.equal(response.status, 200, JSON.stringify(response.body));

    response = await request('PUT', `/v1/config/agents/${AGENT}/resources`, {
      agent_id: 'forged',
      revision: 1,
      inputs: [],
    });
    assert.equal(response.status, 200, JSON.stringify(response.body));
    response = await request('PUT', `/v1/config/agents/${AGENT}/resources`, {
      agent_id: AGENT,
      revision: 2,
      inputs: [],
    });
    assert.equal(response.status, 200, JSON.stringify(response.body));

    response = await request('POST', `/v1/config/agents/${AGENT}/publish`);
    assert.equal(response.status, 200, JSON.stringify(response.body));
    const fingerprint = response.body.fingerprint;

    const webhookId = `webhook-${process.pid}`;
    response = await request('PUT', `/v1/config/webhook-subscriptions/${webhookId}`, {});
    assert.equal(response.status, 400);
    response = await request('PUT', `/v1/config/webhook-subscriptions/${webhookId}`, {
      url: 'http://127.0.0.1/private',
      event_types: ['session.created'],
    });
    assert.equal(response.status, 400);
    response = await request('PUT', `/v1/config/webhook-subscriptions/${webhookId}`, {
      url: 'https://hooks.example.invalid/awaken',
      event_types: ['session.created', 3, 'session.ended'],
    });
    // Cause/effect rules: every event type is a non-empty string => create;
    // a mixed-type array => reject the complete request without coercion.
    assert.equal(response.status, 400, JSON.stringify(response.body));
    response = await request('PUT', `/v1/config/webhook-subscriptions/${webhookId}`, {
      url: 'https://hooks.example.invalid/awaken',
      event_types: ['session.created', 'session.ended'],
    });
    assert.equal(response.status, 201, JSON.stringify(response.body));
    assert.match(response.body.secret, /^whsec_/);
    response = await request('PUT', `/v1/config/webhook-subscriptions/${webhookId}`, {
      url: 'https://hooks.example.invalid/updated',
      event_types: ['session.ended'],
    });
    assert.equal(response.status, 200, JSON.stringify(response.body));
    assert.equal(response.body.secret, undefined);
    response = await request('GET', `/v1/config/webhook-subscriptions/${webhookId}`);
    assert.equal(response.status, 200, JSON.stringify(response.body));
    response = await request('GET', '/v1/config/webhook-subscriptions');
    assert.equal(response.status, 200, JSON.stringify(response.body));
    assert.ok(response.body.data.some((row: any) => row.id === webhookId));
    response = await request('DELETE', `/v1/config/webhook-subscriptions/${webhookId}`);
    assert.equal(response.status, 204);
    response = await request('GET', `/v1/config/webhook-subscriptions/${webhookId}`);
    assert.equal(response.status, 404);

    await executePublishedAgent(1);
    assert.equal(upstream.requests.length, 1);

    await stopServer(server);
    server = start(directory, database.url);
    await waitForPort(PORT, 120_000, server);

    response = await request('GET', `/v1/config/agents/${AGENT}`);
    assert.equal(response.status, 200, JSON.stringify(response.body));
    response = await request('GET', '/v1/config/catalog');
    assert.equal(response.status, 200, JSON.stringify(response.body));
    assert.ok(response.body.offerings.some((offering: any) => offering.model_id === MODEL));
    response = await request('POST', `/v1/config/agents/${AGENT}/publish`);
    assert.equal(response.status, 200, JSON.stringify(response.body));
    assert.equal(response.body.fingerprint, fingerprint);

    await executePublishedAgent(2);
    assert.equal(upstream.requests.length, 2);

    for (const valid of [
      {
        kind: 'oauth',
        provider_id: PROVIDER,
        oauth_helper: 'gcloud',
      },
    ]) {
      response = await request('POST', '/v1/config/credentials', {
        workspace_id: WORKSPACE,
        ...valid,
      });
      assert.equal(response.status, 201, JSON.stringify(response.body));
      assert.equal(response.body.kind, valid.kind);
    }

    // Cause graph / decision table for credential ownership after restart:
    // operator-owned managed material -> public authoring API; host-observed
    // WorkerLocal identity -> discovery/ensure_worker_local only. Routing both
    // through POST would create two registration authorities.
    //
    // | kind         | public POST | authoritative registration |
    // | oauth        | 201         | credential API             |
    // | worker_local | 422         | local discovery            |
    response = await request('POST', '/v1/config/credentials', {
      workspace_id: WORKSPACE,
      kind: 'worker_local',
      provider_id: PROVIDER,
    });
    assert.equal(response.status, 422, JSON.stringify(response.body));
    assert.equal(response.body.code, 'credential_invalid');

    console.log(
      'POSTGRES CONTROL PLANE TS E2E PASS: catalog, credentials, Agent publication, admin resources/webhooks, and Sessions survive one process replacement.',
    );
  } finally {
    await stopServer(server);
    upstream.close();
    fs.rmSync(directory, { recursive: true, force: true });
    if (database.owned) docker('rm', '-f', database.container);
  }
}

main().catch((error) => {
  console.error(error);
  process.exitCode = 1;
});
