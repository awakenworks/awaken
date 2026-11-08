import assert from 'node:assert/strict';
import http from 'node:http';
import { test } from 'node:test';
import {
  MODEL_UPSTREAM,
  assertNoModelEnvironment,
  configureModelUpstream,
  modelEnvironmentVariables,
  modelUpstreamEnv,
} from './model_upstream.mjs';

test('a scenario process environment never carries model configuration', () => {
  // Decision rules M1 *_API_KEY / API_KEY in any case => refused; M2
  // AWAKEN_MODEL_SOURCE => refused; M3 every other variable (including a
  // decoy model name a test passes to prove it is ignored) => accepted.
  const refused = ['API_KEY', 'ANTHROPIC_API_KEY', 'gemini_api_key', 'AWAKEN_MODEL_SOURCE'];
  const accepted = { PATH: '/bin', AWAKEN_MODEL_MODE: 'real', GEMINI_MODEL: 'decoy' };
  const environment = { ...accepted, ...Object.fromEntries(refused.map((name) => [name, 'x'])) };
  assert.deepEqual(modelEnvironmentVariables(environment).sort(), [...refused].sort());
  assert.throws(() => assertNoModelEnvironment(environment), /AWAKEN_MODEL_SOURCE/u);
  assert.doesNotThrow(() => assertNoModelEnvironment(accepted));
});

test('a model upstream binding rides an env object without becoming a variable', () => {
  // C1 spread into a larger env keeps the binding; C2 the string keys a child
  // process receives never include it.
  const upstream = { url: 'http://127.0.0.1:1' };
  const env = { A: '1', ...modelUpstreamEnv(upstream, { model: 'm' }) };
  assert.deepEqual({ ...env }[MODEL_UPSTREAM], { upstream, model: 'm' });
  assert.deepEqual(Object.keys(env), ['A']);
});

async function configPlane(handler, authorizations = []) {
  const calls = [];
  const server = http.createServer((req, res) => {
    authorizations.push(req.headers.authorization);
    let body = '';
    req.on('data', (chunk) => { body += chunk; });
    req.on('end', () => {
      const call = { method: req.method, url: req.url, body: body ? JSON.parse(body) : null };
      calls.push(call);
      const [status, json] = handler(call);
      res.writeHead(status, { 'content-type': 'application/json' });
      res.end(JSON.stringify(json));
    });
  });
  await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve));
  return { url: `http://127.0.0.1:${server.address().port}`, calls, close: () => server.close() };
}

test('configuration connects the provider, then binds every profile to an offered model', async () => {
  // U1 offered => connection in the named Workspace, then one PUT per profile
  // with the exact credential;
  // U3 a profile model the connection did not offer => throws before binding it.
  const offerings = [{ model_id: 'fake-haiku', protocol_endpoint_id: 'ep' }];
  const plane = await configPlane(({ method, url }) => {
    if (method === 'POST') return [201, { endpoint: { id: 'ep' }, credential: { id: 'cred' } }];
    if (url === '/v1/config/catalog') return [200, { offerings }];
    if (url === '/v1/config/workspace-context') return [200, { workspace_id: 'ws' }];
    return [200, {}];
  });
  try {
    const upstream = { url: 'http://fake.invalid', models: [] };
    const bound = await configureModelUpstream(plane.url, { upstream, apiKey: 'k', workspaceId: 'ws' });
    assert.deepEqual(upstream.models, ['fake-haiku'], 'the fake directory offers the bound model');
    assert.deepEqual(bound, { endpointId: 'ep', credentialId: 'cred', profiles: { default: 'fake-haiku' }, model: 'fake-haiku' });
    const put = plane.calls.find((call) => call.method === 'PUT');
    assert.equal(put.url, '/v1/config/inference-profiles/default');
    assert.deepEqual(put.body.primary.credential_binding, { type: 'exact', credential_source_id: 'cred' });
    const connection = plane.calls.find((call) => call.method === 'POST');
    assert.equal(connection.body.secret, 'k');
    assert.equal(connection.body.workspace_id, 'ws', 'claims the named Workspace');
    assert.equal(put.body.workspace_id, 'ws');
    await assert.rejects(
      configureModelUpstream(plane.url, { upstream, apiKey: 'k', profiles: { other: 'absent' } }),
      /offers no model absent/u,
    );
  } finally {
    plane.close();
  }
});

test('a refused connection fails without echoing the key', async () => {
  // U2: the API problem is surfaced, the secret redacted.
  const plane = await configPlane(() => [422, { detail: 'bad key super-secret' }]);
  try {
    await assert.rejects(
      configureModelUpstream(plane.url, { baseUrl: 'http://p.invalid/v1/', apiKey: 'super-secret' }), // awaken-allow(secret): inert fixture
      (error) => /422/u.test(error.message) && !error.message.includes('super-secret'),
    );
  } finally {
    plane.close();
  }
});

test('a token-scoped operator authenticates and claims its token Workspace', async () => {
  // T1 authTokenFile => every call carries the bearer, and the connection
  // claims the Workspace the context route reports for that token.
  const fs = await import('node:fs');
  const os = await import('node:os');
  const path = await import('node:path');
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'model-upstream-token-'));
  const tokenFile = path.join(dir, 'admin-token');
  fs.writeFileSync(tokenFile, 'tok\n');
  const offerings = [{ model_id: 'fake-haiku', protocol_endpoint_id: 'ep' }];
  const headers = [];
  const plane = await configPlane(({ method, url }) => {
    if (method === 'POST') return [201, { endpoint: { id: 'ep' }, credential: { id: 'cred' } }];
    if (url === '/v1/config/workspace-context') return [200, { workspace_id: 'token-ws' }];
    if (url === '/v1/config/catalog') return [200, { offerings }];
    return [200, {}];
  }, headers);
  try {
    await configureModelUpstream(plane.url, { baseUrl: 'http://p.invalid/v1/', apiKey: 'k', authTokenFile: tokenFile });
    assert.ok(headers.length > 0 && headers.every((value) => value === 'Bearer tok'));
    assert.equal(plane.calls.find((call) => call.method === 'POST').body.workspace_id, 'token-ws');
  } finally {
    plane.close();
    fs.rmSync(dir, { recursive: true, force: true });
  }
});
