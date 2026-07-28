// Live DeepSeek BYOK proof through the production provider composition.
//
// Cause graph / decision table:
// valid key + /models + active model -> atomically Ready -> exact Profile ->
// auto Agent publication -> Chat Completions turn; invalid/missing key fails or
// skips before persisted execution. The write-only key must never enter output.

import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import Anthropic from '@anthropic-ai/sdk';
import { deploymentEnv, spawnServer, stopServer, waitForPort, pass } from './harness.mjs';

const PORT = Number(process.env.E2E_PORT ?? 38317);
const KEY = process.env.DEEPSEEK_API_KEY;
const BETAS = ['managed-agents-2026-04-01'];

async function request(base, method, uri, body) {
  const response = await fetch(`${base}${uri}`, {
    method,
    headers: body === undefined ? {} : { 'content-type': 'application/json' },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  const text = await response.text();
  return { status: response.status, body: text ? JSON.parse(text) : null };
}

async function main() {
  if (!KEY) {
    console.log('SKIP provider_connection_deepseek_real_e2e: no DEEPSEEK_API_KEY set.');
    return;
  }
  const directory = fs.mkdtempSync(path.join(os.tmpdir(), 'awaken-deepseek-live-'));
  const env = deploymentEnv(directory);
  const { server } = spawnServer('management-providers', PORT, env);
  try {
    await waitForPort(PORT, 180_000, server);
    const base = `http://127.0.0.1:${PORT}`;
    const workspace = fs.readFileSync(path.join(directory, 'platform-workspace-id'), 'utf8').trim();
    let result = await request(base, 'POST', '/v1/config/provider-connections', {
      workspace_id: workspace,
      provider_id: 'deepseek',
      display_name: 'DeepSeek',
      endpoint_id: 'deepseek-live-chat',
      dialect: 'open_ai_chat',
      base_url: 'https://api.deepseek.com',
      timeout_secs: 90,
      secret: KEY,
    });
    assert.equal(result.status, 201, JSON.stringify(result.body));
    assert.ok(result.body.sync.discovered > 0, JSON.stringify(result.body));
    assert.ok(!JSON.stringify(result.body).includes(KEY), 'connection response is secret-free');
    const credentialId = result.body.credential.id;
    pass(`DeepSeek /models discovered ${result.body.sync.discovered} active model(s)`);

    result = await request(base, 'GET', '/v1/config/catalog');
    assert.equal(result.status, 200, JSON.stringify(result.body));
    const offerings = result.body.offerings.filter(
      (offering) => offering.provider_id === 'deepseek' && (offering.status ?? 'active') === 'active',
    );
    const offering = offerings.find((item) => item.model_id === 'deepseek-v4-flash') ?? offerings[0];
    assert.ok(offering, JSON.stringify(result.body));

    result = await request(base, 'PUT', '/v1/config/inference-profiles/deepseek-route', {
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
    assert.equal(result.status, 200, JSON.stringify(result.body));

    const agent = 'deepseek-live-agent';
    result = await request(base, 'PUT', `/v1/config/agents/${agent}`, {
      id: agent,
      name: 'DeepSeek Live E2E',
      system: 'Follow the user instruction exactly and answer briefly.',
      max_steps: 2,
      model: { mode: 'auto' },
      tools: [],
    });
    assert.equal(result.status, 200, JSON.stringify(result.body));
    result = await request(base, 'POST', `/v1/config/agents/${agent}/publish`);
    assert.equal(result.status, 200, JSON.stringify(result.body));
    assert.equal(result.body.installed, true);

    const client = new Anthropic({ apiKey: 'local-e2e', baseURL: base }); // awaken-allow: secret (local protocol fixture)
    const session = await client.beta.sessions.create({ agent, environment_id: 'env_local', betas: BETAS });
    await client.beta.sessions.events.send(session.id, {
      events: [{ type: 'user.message', content: [{ type: 'text', text: 'Reply with exactly AWAKEN-DEEPSEEK-LIVE-OK' }] }],
      betas: BETAS,
    });
    const events = [];
    for await (const event of client.beta.sessions.events.list(session.id, { betas: BETAS })) events.push(event);
    assert.match(JSON.stringify(events), /AWAKEN-DEEPSEEK-LIVE-OK/u);
    pass(`DeepSeek ${offering.model_id} completed an Awaken managed turn through the saved BYOK Profile`);
    console.log('E2E PASS: native DeepSeek Provider Connection -> Profile -> Agent -> live managed turn.');
  } finally {
    await stopServer(server).catch(() => {});
    fs.rmSync(directory, { recursive: true, force: true });
  }
}

main().catch((error) => {
  console.error('E2E FAIL:', error);
  process.exitCode = 1;
});
