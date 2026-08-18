// Real provider/dialect certification lane.
//
// Built-in credentials are discovered from their conventional environment
// variables. Arbitrary third-party providers are supplied through the
// secret-name-only AWAKEN_PROVIDER_CASES_JSON contract; secret bytes are read
// from the named environment variable and never enter the emitted artifact.

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
} from './harness.mjs';
import { loadProviderCases, publicProviderCase } from './provider_compat_cases.mjs';

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

async function managedTurn(base, agent, marker) {
  const client = new Anthropic({ apiKey: 'local-provider-certification', baseURL: base }); // awaken-allow: secret (local fixture)
  const session = await client.beta.sessions.create({
    agent,
    environment_id: 'env_local',
    betas: BETAS,
  });
  await client.beta.sessions.events.send(session.id, {
    events: [{
      type: 'user.message',
      content: [{ type: 'text', text: `Reply with exactly ${marker}` }],
    }],
    betas: BETAS,
  });
  const events = [];
  for await (const event of client.beta.sessions.events.list(session.id, { betas: BETAS })) {
    events.push(event);
  }
  assert.match(JSON.stringify(events), new RegExp(marker, 'u'));
  assert.ok(events.some((event) => event.type === 'session.status_idle'));
  await client.beta.sessions.delete(session.id, { betas: BETAS });
  // DELETE acknowledges the lifecycle transition before asynchronous resource
  // cleanup drains. Keep the owned server alive long enough to observe that
  // boundary instead of cancelling cleanup during fixture teardown.
  await new Promise((resolve) => setTimeout(resolve, 500));
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
    result = await request(base, 'PUT', `/v1/config/agents/${agent}`, {
      id: agent,
      name: `Provider certification ${item.id}`,
      system: 'Follow the user instruction exactly and answer briefly.',
      max_steps: 2,
      model: { mode: 'auto' },
      tools: [],
    });
    assert.equal(result.status, 200, `${item.id}: author agent`);
    result = await request(base, 'POST', `/v1/config/agents/${agent}/publish`);
    assert.equal(result.status, 200, `${item.id}: publish agent`);
    assert.equal(result.body.installed, true, `${item.id}: agent was not installed`);

    const marker = `AWAKEN-PROVIDER-${item.id.toUpperCase()}-OK`;
    await managedTurn(base, agent, marker);
    pass(`${item.id}: discovery -> exact credential -> publication -> real Managed turn`);
    return {
      ...publicProviderCase(item),
      selected_model: offering.model_id,
      discovered_models: discoveredModels,
      latency_ms: performance.now() - started,
      status: 'passed',
    };
  } finally {
    await stopServer(server).catch(() => {});
    fs.rmSync(directory, { recursive: true, force: true });
  }
}

async function main() {
  const cases = loadProviderCases();
  if (cases.length === 0) {
    if (process.env.AWAKEN_PROVIDER_MATRIX_REQUIRE_CASES === '1') {
      throw new Error('provider certification requires at least one configured provider case');
    }
    console.log('SKIP provider_connection_matrix_real_e2e: no provider credentials configured.');
    return;
  }
  const results = [];
  for (const [index, item] of cases.entries()) results.push(await certifyProvider(item, index));
  console.log(`AWAKEN_PROVIDER_COMPAT ${JSON.stringify({ version: 1, results })}`);
  console.log(`E2E PASS: ${results.length} real provider/dialect compatibility case(s).`);
}

main().catch((error) => {
  console.error('E2E FAIL:', error);
  process.exitCode = 1;
});
