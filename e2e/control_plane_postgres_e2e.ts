// Real-process E2E for the complete Postgres control plane.
//
// Catalog, credential, Agent publication, admin resources/webhooks, and Session
// state are independent repository ports. This scenario selects their Postgres
// adapters together, drives only public HTTP, restarts the process, and proves the
// same immutable publication still executes without a node-local config truth.

import assert from 'node:assert/strict';
import fs from 'node:fs';
import net from 'node:net';
import os from 'node:os';
import path from 'node:path';
import { execFileSync, execSync, spawn, type ChildProcess } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import Anthropic from '@anthropic-ai/sdk';
import { startFakeAnthropic } from './fixtures/fake_anthropic_fixture.mjs';
import { deploymentEnv } from './harness.mjs';

const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const PORT = Number(process.env.E2E_PORT ?? 39413);
const WORKSPACE = `workspace_control_pg_${process.pid}`;
const AGENT = `control-pg-agent-${process.pid}`;
const MODEL = 'fake-haiku';
const PROVIDER = 'anthropic';
const ENDPOINT = `control-pg-endpoint-${process.pid}`;
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

function awakenBin(): string {
  const output = execSync('cargo build --quiet --message-format=json -p awaken-cli --bin awaken', {
    cwd: ROOT,
    maxBuffer: 64 * 1024 * 1024,
  }).toString();
  for (const line of output.split('\n')) {
    if (!line.trim()) continue;
    try {
      const message = JSON.parse(line);
      if (message.executable && message.target?.name === 'awaken') return message.executable;
    } catch {
      // Cargo may emit non-JSON diagnostics.
    }
  }
  throw new Error('could not resolve the awaken binary path');
}

function start(bin: string, directory: string, databaseUrl: string): ChildProcess {
  return spawn(bin, ['serve', '--port', String(PORT)], {
    env: {
      ...process.env,
      ...deploymentEnv(directory, {
        controlSealKey: SEAL_KEY,
        databases: {
          catalog_db: databaseUrl,
          credential_db: databaseUrl,
          config_db: databaseUrl,
          admin_db: databaseUrl,
          sessions_db: databaseUrl,
        },
      }),
      AWAKEN_SCENARIO_WORKSPACE: WORKSPACE,
    },
    stdio: ['ignore', 'ignore', 'inherit'],
  });
}

async function waitUntilReady(): Promise<void> {
  const deadline = Date.now() + 60_000;
  while (Date.now() < deadline) {
    const connected = await new Promise<boolean>((resolve) => {
      const socket = net.createConnection({ host: '127.0.0.1', port: PORT });
      socket.once('connect', () => {
        socket.destroy();
        resolve(true);
      });
      socket.once('error', () => {
        socket.destroy();
        resolve(false);
      });
    });
    if (connected) return;
    await sleep(100);
  }
  throw new Error('Postgres control-plane process did not become ready');
}

async function stop(child: ChildProcess): Promise<void> {
  if (child.exitCode !== null) return;
  child.kill('SIGINT');
  await new Promise((resolve) => child.once('exit', resolve));
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
  await client.beta.sessions.events.send(session.id, {
    events: [{ type: 'user.message', content: [{ type: 'text', text: `postgres-${expectedCalls}` }] }],
    betas: BETAS,
  });
  const events = [];
  for await (const event of client.beta.sessions.events.list(session.id, { betas: BETAS })) {
    events.push(event);
  }
  const message: any = events.find((event: any) => event.type === 'agent.message');
  const text = (message?.content ?? []).map((part: any) => part.text ?? '').join('');
  assert.match(text, new RegExp(`FAKE:postgres-${expectedCalls}`));
}

async function main(): Promise<void> {
  const database = await postgres();
  const upstream = await startFakeAnthropic(FAKE_KEY, { models: [MODEL] });
  const directory = fs.mkdtempSync(path.join(os.tmpdir(), 'awaken-control-pg-'));
  const bin = awakenBin();
  let server = start(bin, directory, database.url);

  try {
    await waitUntilReady();

    let response = await request('POST', '/v1/config/provider-connections', {
      workspace_id: WORKSPACE,
      provider_id: PROVIDER,
      display_name: 'Postgres provider',
      endpoint_id: ENDPOINT,
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

    await stop(server);
    server = start(bin, directory, database.url);
    await waitUntilReady();

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
    await stop(server);
    upstream.close();
    fs.rmSync(directory, { recursive: true, force: true });
    if (database.owned) docker('rm', '-f', database.container);
  }
}

main().catch((error) => {
  console.error(error);
  process.exitCode = 1;
});
