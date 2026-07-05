// The REAL provider path without a live key: a fake Anthropic-compatible
// upstream serves deterministic replies, so (a) `AWAKEN_MODEL_MODE=real`
// drives GenaiExecutor over the actual wire for a multi-turn conversation, and
// (b) the management plane's credential-validation probe exercises its
// valid/invalid arms. Deterministic, hermetic, CI-safe.

import assert from 'node:assert/strict';
import Anthropic from '@anthropic-ai/sdk';
import { withServer, pass } from './harness.mjs';
import { startFakeAnthropic } from './fixtures/fake_anthropic_fixture.mjs';

const BETAS = ['managed-agents-2026-04-01'];
const FAKE_KEY = 'sk-fake-upstream-key'; // awaken-allow: secret

async function listEvents(client, sessionId) {
  const events = [];
  for await (const ev of client.beta.sessions.events.list(sessionId, { betas: BETAS })) events.push(ev);
  return events;
}

function agentMessages(events) {
  return events
    .filter((e) => e.type === 'agent.message')
    .map((e) => e.content.map((b) => b.text ?? '').join(''));
}

async function req(base, method, uri, body) {
  const res = await fetch(`${base}${uri}`, {
    method,
    headers: body === undefined ? {} : { 'content-type': 'application/json' },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  const text = await res.text();
  return { status: res.status, json: text ? JSON.parse(text) : null };
}

async function main() {
  const upstream = await startFakeAnthropic(FAKE_KEY);
  try {
    // ---- arm 1: AWAKEN_MODEL_MODE=real over the fake upstream --------------
    process.env.ANTHROPIC_API_KEY = FAKE_KEY;
    process.env.ANTHROPIC_BASE_URL = `${upstream.url}/v1/`;
    process.env.ANTHROPIC_MODEL = 'fake-haiku';
    await withServer('real', 38194, async (baseUrl) => {
      const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: baseUrl });
      const session = await client.beta.sessions.create({ agent: 'assistant', betas: BETAS });
      await client.beta.sessions.events.send(session.id, {
        betas: BETAS,
        events: [{ type: 'user.message', content: [{ type: 'text', text: 'first turn' }] }],
      });
      await client.beta.sessions.events.send(session.id, {
        betas: BETAS,
        events: [{ type: 'user.message', content: [{ type: 'text', text: 'second turn' }] }],
      });
      const events = await listEvents(client, session.id);
      const replies = agentMessages(events);
      assert.ok(
        replies.some((m) => m.includes('FAKE:first turn')),
        `turn 1 reply came from the wire: ${JSON.stringify(replies)}`,
      );
      assert.ok(
        replies.some((m) => m.includes('FAKE:second turn')),
        'turn 2 reply came from the wire',
      );
      pass('real mode over the fake upstream: multi-turn via GenaiExecutor on the wire');
    });
    assert.ok(upstream.requests.length >= 2, 'the upstream served the chat turns');
    assert.equal(upstream.unauthorized, 0, 'the key rode every chat request');
    pass(`upstream served ${upstream.requests.length} authorized /messages calls`);

    // ---- arm 2: the credential-validation probe, valid + invalid -----------
    delete process.env.ANTHROPIC_API_KEY;
    delete process.env.ANTHROPIC_BASE_URL;
    delete process.env.ANTHROPIC_MODEL;
    await withServer('management', 38195, async (base) => {
      await req(base, 'PUT', '/v1/config/providers/anthropic', {
        id: 'anthropic', slug: 'anthropic', display_name: 'Anthropic', version: 1,
      });
      await req(base, 'PUT', '/v1/config/endpoints/ep1', {
        id: 'ep1', provider_id: 'anthropic', flavor: 'anthropic_messages',
        base_url: `${upstream.url}/v1/`, timeout_secs: 300, display_name: 'fake', version: 1,
      });
      await req(base, 'POST', '/v1/config/offerings', {
        model_id: 'fake-haiku', provider_id: 'anthropic',
        protocol_endpoint_id: 'ep1', flavor: 'anthropic_messages', upstream_model: null,
      });

      let r = await req(base, 'POST', '/v1/config/credentials', {
        workspace_id: 'ws', kind: 'vault', provider_id: 'anthropic',
        env_key: 'ANTHROPIC_API_KEY', secret: FAKE_KEY,
      });
      assert.equal(r.status, 201);
      r = await req(base, 'POST', `/v1/config/credentials/${r.json.id}/validate`, {
        workspace_id: 'ws', model_id: 'fake-haiku',
      });
      assert.equal(r.status, 200, JSON.stringify(r.json));
      assert.equal(r.json.status, 'valid', `good key probes valid: ${JSON.stringify(r.json)}`);
      pass('probe: good key -> valid (live probe over the fake upstream)');

      r = await req(base, 'POST', '/v1/config/credentials', {
        workspace_id: 'ws', kind: 'vault', provider_id: 'anthropic',
        env_key: 'ANTHROPIC_API_KEY',
        secret: 'sk-wrong-key', // awaken-allow: secret
      });
      assert.equal(r.status, 201);
      r = await req(base, 'POST', `/v1/config/credentials/${r.json.id}/validate`, {
        workspace_id: 'ws', model_id: 'fake-haiku',
      });
      assert.equal(r.status, 200);
      assert.equal(r.json.status, 'invalid', `wrong key probes invalid: ${JSON.stringify(r.json)}`);
      pass('probe: wrong key -> invalid (never a false valid)');
    });

    console.log('E2E PASS: real provider wire + validation probe, hermetic via the fake upstream.');
  } finally {
    upstream.close();
  }
}

main().catch((err) => {
  console.error('E2E FAIL:', err);
  process.exit(1);
});
