// can_consume validity join (E3-3), end-to-end over HTTP against the management
// config plane. A credential scoped to one provider must not authenticate another
// provider's model: resolving such a binding is fail-closed (422
// incompatible_credential), while a compatible or unscoped credential resolves.
// CI-safe: discovery uses a deterministic local provider fixture; inference is
// never invoked.

import assert from 'node:assert/strict';
import http from 'node:http';
import { withServer, pass } from './harness.mjs';

const PORT = Number(process.env.E2E_PORT ?? 38471);
const WS = 'ws';
const MODEL = 'fake-haiku';
const PROVIDER_KEY = 'sk-validity-fixture'; // awaken-allow: secret

async function modelDirectory() {
  const server = http.createServer((_request, response) => {
    response.writeHead(200, { 'content-type': 'application/json' });
    response.end(JSON.stringify({ data: [{ id: MODEL }], has_more: false }));
  });
  await new Promise((resolve, reject) => {
    server.once('error', reject);
    server.listen(0, '127.0.0.1', resolve);
  });
  const address = server.address();
  assert.ok(address && typeof address === 'object');
  return {
    url: `http://127.0.0.1:${address.port}`,
    close: () => new Promise((resolve) => server.close(resolve)),
  };
}

async function main() {
  const upstream = await modelDirectory();
  try {
    await withServer('management', PORT, async (baseUrl) => {
    const cfg = async (method, path, body) => {
      const res = await fetch(`${baseUrl}${path}`, {
        method,
        headers: { 'content-type': 'application/json' },
        body: body === undefined ? undefined : JSON.stringify(body),
      });
      return { status: res.status, body: await res.json().catch(() => ({})) };
    };

    const connection = await cfg('POST', '/v1/config/provider-connections', {
      workspace_id: WS,
      provider_id: 'anthropic',
      display_name: 'Anthropic',
      endpoint_id: 'ep1',
      dialect: 'anthropic_messages',
      base_url: `${upstream.url}/v1/`,
      timeout_secs: 300,
      secret: PROVIDER_KEY,
    });
    assert.equal(connection.status, 201, `provider connection: ${JSON.stringify(connection.body)}`);
    pass(`connected anthropic and discovered ${MODEL}`);

    // ── A credential scoped to a DIFFERENT provider (openai) ────────────────
    const foreign = await cfg('POST', '/v1/config/credentials', {
      workspace_id: WS,
      kind: 'vault',
      provider_id: 'openai',
      env_key: 'OPENAI_API_KEY',
      secret: 'sk-openai-xxxxx', // awaken-allow: secret
    });
    assert.equal(foreign.status, 201, 'foreign credential created');
    const foreignId = foreign.body.id;

    // Resolving the anthropic model with the openai key is fail-closed (422).
    const bad = await cfg('POST', '/v1/config/inference/resolve', {
      workspace_id: WS,
      target: { model_id: MODEL },
      binding: { type: 'exact', credential_source_id: foreignId },
    });
    assert.equal(
      bad.status,
      422,
      `a provider-mismatched credential is rejected 422 (got ${bad.status}: ${JSON.stringify(bad.body)})`,
    );
    assert.ok(
      JSON.stringify(bad.body).includes('incompatible_credential'),
      `the problem body names the incompatible_credential code (got ${JSON.stringify(bad.body)})`,
    );
    pass('resolve(anthropic model, openai key) -> 422 incompatible_credential (can_consume fail-closed)');

    // ── A compatible (anthropic-scoped) credential resolves ─────────────────
    const good = await cfg('POST', '/v1/config/credentials', {
      workspace_id: WS,
      kind: 'vault',
      provider_id: 'anthropic',
      env_key: 'ANTHROPIC_API_KEY',
      secret: 'sk-anthropic-xxxxx', // awaken-allow: secret
    });
    assert.equal(good.status, 201, 'compatible credential created');
    const okResolve = await cfg('POST', '/v1/config/inference/resolve', {
      workspace_id: WS,
      target: { model_id: MODEL },
      binding: { type: 'exact', credential_source_id: good.body.id },
    });
    assert.equal(
      okResolve.status,
      200,
      `a provider-matched credential resolves (got ${okResolve.status}: ${JSON.stringify(okResolve.body)})`,
    );
    assert.equal(okResolve.body.credential_present, true, 'the compatible credential materialized');
    pass('resolve(anthropic model, anthropic key) -> 200 (can_consume permits the match)');

    // ── An unscoped (provider-less) credential consumes any provider ─────────
    // A vault credential with no provider_id: it materializes from the stored
    // secret AND passes can_consume for any provider.
    const unscoped = await cfg('POST', '/v1/config/credentials', {
      workspace_id: WS,
      kind: 'vault',
      env_key: 'ANY_KEY',
      secret: 'sk-unscoped-xxxxx', // awaken-allow: secret
    });
    assert.equal(unscoped.status, 201, 'unscoped credential created');
    const anyResolve = await cfg('POST', '/v1/config/inference/resolve', {
      workspace_id: WS,
      target: { model_id: MODEL },
      binding: { type: 'exact', credential_source_id: unscoped.body.id },
    });
    assert.equal(
      anyResolve.status,
      200,
      `an unscoped credential consumes any provider (got ${anyResolve.status}: ${JSON.stringify(anyResolve.body)})`,
    );
    pass('resolve(anthropic model, unscoped env key) -> 200 (unscoped consumes any provider)');

    console.log('E2E PASS: can_consume validity join fail-closes provider mismatch and permits compatible/unscoped keys.');
    });
  } finally {
    await upstream.close();
  }
  process.exitCode = 0;
}

main().catch((err) => {
  console.error('E2E FAIL:', err);
  process.exitCode = 1;
});
