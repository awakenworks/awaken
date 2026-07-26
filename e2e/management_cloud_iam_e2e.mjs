// Awaken Cloud identity over the production awaken-iam remote protocol. The
// fixture is an IAM service boundary (JWKS + PDP), not an awaken test endpoint:
// the spawned product still performs real JWT verification, workspace routing,
// PEP enforcement, and remote authorization over HTTP.
//
// Brokered model cause graph / decision table:
// C1 valid cached Cloud login, C2 Cloud readiness, C3 native model projection,
// C4 explicit brokered Profile binding, C5 later model removal.
// T1 C1+C2+C3 -> active brokered Offering + Cloud-provenance token metadata.
// T2 C1+C2+C3+C4 -> exact credential-free publication preview (never `none`).
// T3 C1+C2+C5 -> Offering unavailable and only stale brokered metadata removed.

import assert from 'node:assert/strict';
import { generateKeyPairSync, sign } from 'node:crypto';
import fs from 'node:fs';
import http from 'node:http';
import os from 'node:os';
import path from 'node:path';
import { deploymentEnv, spawnServer, stopServer, waitForPort, pass, startUpstream, realServerEnv } from './harness.mjs';

const PORT = 38257;
const ISSUER = 'https://accounts.e2e.awakenworks.test';
const AUDIENCE = 'awaken-runtime';
const KEY_ID = 'cloud-e2e-ed25519';
const SERVICE_TOKEN = 'cloud-e2e-service-credential'; // awaken-allow: secret
const SEAL_KEY = '12233445566778899aabbccddeeff00112233445566778899aabbccddeeff001';

function b64url(value) {
  return Buffer.from(value).toString('base64url');
}

function accessToken(privateKey, subject, { expiresIn = 3600, audience = AUDIENCE } = {}) {
  const now = Math.floor(Date.now() / 1000);
  const header = b64url(JSON.stringify({ alg: 'EdDSA', typ: 'JWT', kid: KEY_ID }));
  const payload = b64url(JSON.stringify({
    iss: ISSUER,
    sub: subject,
    aud: audience,
    exp: now + expiresIn,
    iat: now,
    jti: `jti-${subject}-${expiresIn}`,
  }));
  const input = `${header}.${payload}`;
  return `${input}.${sign(null, Buffer.from(input), privateKey).toString('base64url')}`;
}

async function startIamFixture() {
  const { publicKey, privateKey } = generateKeyPairSync('ed25519');
  const publicJwk = publicKey.export({ format: 'jwk' });
  const calls = [];
  const cloudCalls = [];
  let decision = 'allow';
  let cloudModels = [{
    provider: 'openai',
    original_model_id: 'gpt-5-e2e',
    native_protocol: 'openai_responses',
    context_window: 400000,
    max_output_tokens: 128000,
    capabilities: ['responses'],
    route_publication_revision: 7,
  }];
  const server = http.createServer(async (request, response) => {
    if (request.method === 'GET' && request.url === '/.well-known/jwks.json') {
      response.setHeader('content-type', 'application/json');
      response.end(JSON.stringify({
        keys: [{
          kty: 'OKP', crv: 'Ed25519', x: publicJwk.x, kid: KEY_ID, use: 'sig', alg: 'EdDSA',
        }],
      }));
      return;
    }
    if (request.method === 'GET' && request.url === '/v1/inference/readiness') {
      cloudCalls.push({ path: request.url, authorization: request.headers.authorization });
      response.setHeader('content-type', 'application/json');
      response.end(JSON.stringify({ ready: true }));
      return;
    }
    if (request.method === 'GET' && request.url === '/v1/inference/models') {
      cloudCalls.push({ path: request.url, authorization: request.headers.authorization });
      response.setHeader('content-type', 'application/json');
      response.end(JSON.stringify({ data: cloudModels }));
      return;
    }
    if (request.method === 'POST' && request.url === '/v1/authorize') {
      const chunks = [];
      for await (const chunk of request) chunks.push(chunk);
      const body = JSON.parse(Buffer.concat(chunks).toString('utf8'));
      calls.push({
        body,
        authorization: request.headers.authorization,
        audience: request.headers['x-iam-audience'],
      });
      response.setHeader('content-type', 'application/json');
      response.end(JSON.stringify({
        decision,
        reason: `e2e_${decision}`,
        matched_grants: decision === 'allow' ? ['grant-e2e'] : [],
        matched_roles: decision === 'allow' ? ['role-e2e'] : [],
      }));
      return;
    }
    response.statusCode = 404;
    response.end();
  });
  await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve));
  const address = server.address();
  return {
    url: `http://127.0.0.1:${address.port}`,
    token: (subject, options) => accessToken(privateKey, subject, options),
    calls,
    cloudCalls,
    decide(value) { decision = value; },
    setCloudModels(value) { cloudModels = value; },
    close: () => new Promise((resolve) => server.close(resolve)),
  };
}

async function req(base, method, uri, token, { apiKey = false, body } = {}) {
  const headers = {};
  if (token) headers[apiKey ? 'x-api-key' : 'authorization'] = apiKey ? token : `Bearer ${token}`;
  if (body !== undefined) headers['content-type'] = 'application/json';
  const response = await fetch(`${base}${uri}`, {
    method,
    headers,
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  const text = await response.text();
  return { status: response.status, body: text ? JSON.parse(text) : null };
}

async function main() {
  const directory = fs.mkdtempSync(path.join(os.tmpdir(), 'awaken-cloud-iam-e2e-'));
  const iam = await startIamFixture();
  const upstream = await startUpstream('mcp');
  const cachedToken = iam.token('account-cached');
  const explicitToken = iam.token('account-explicit');
  const expiredToken = iam.token('account-expired', { expiresIn: -1 });
  const wrongAudienceToken = iam.token('account-wrong-audience', { audience: 'other-service' });
  let server = null;
  try {
    const env = deploymentEnv(directory, {
      identityMode: 'awaken-cloud',
      controlSealKey: SEAL_KEY,
      fields: { cloud_api_url: iam.url },
      cloudIam: {
        url: iam.url,
        issuer: ISSUER,
        audience: AUDIENCE,
        accessToken: cachedToken,
        serviceToken: SERVICE_TOKEN,
      },
    });
    ({ server } = spawnServer('management', PORT, {
      ...env,
      ...realServerEnv('mcp', upstream, { mode: 'management' }),
    }));
    await waitForPort(PORT, 180_000, server);
    const base = `http://127.0.0.1:${PORT}`;
    const localWorkspace = fs.readFileSync(path.join(directory, 'platform-workspace-id'), 'utf8').trim();

    // No request credential means "use the cloud login cached by this local
    // process". The platform workspace is resolved at the composition edge.
    let result = await req(base, 'GET', '/v1/files');
    assert.equal(result.status, 200, JSON.stringify(result.body));
    let call = iam.calls.at(-1);
    assert.deepEqual(call.body.principal, { kind: 'account', account_id: 'account-cached' });
    assert.deepEqual(call.body.scope, { kind: 'workspace', workspace_id: localWorkspace });
    assert.equal(call.authorization, `Bearer ${SERVICE_TOKEN}`);
    assert.equal(call.audience, AUDIENCE);
    pass('cached cloud login -> resource PEP -> remote PDP with platform workspace');

    // An explicit Bearer or x-api-key overrides the cached login. Workspace path
    // selection is trusted only after the edge rewrite and reaches the PDP as the
    // target; the inner resource service sees only the stamped WorkspaceScope.
    const selectedWorkspace = 'workspace-cloud-selected';
    result = await req(base, 'GET', `/v1/workspaces/${selectedWorkspace}/skills`, explicitToken);
    assert.equal(result.status, 200, JSON.stringify(result.body));
    call = iam.calls.at(-1);
    assert.deepEqual(call.body.principal, { kind: 'account', account_id: 'account-explicit' });
    assert.equal(call.body.action, 'awaken.runtime.resources::skill.read');
    assert.deepEqual(call.body.scope, { kind: 'workspace', workspace_id: selectedWorkspace });
    result = await req(base, 'GET', `/v1/workspaces/${selectedWorkspace}/memory_stores`, explicitToken, { apiKey: true });
    assert.equal(result.status, 200, JSON.stringify(result.body));
    pass('explicit Bearer/x-api-key and workspace path use the same cloud PEP');

    result = await req(base, 'GET', `/v1/workspaces/${selectedWorkspace}/config/catalog`, explicitToken);
    assert.equal(result.status, 200, JSON.stringify(result.body));
    call = iam.calls.at(-1);
    assert.equal(call.body.action, 'awaken.runtime.management::workspace.read');
    assert.deepEqual(call.body.scope, { kind: 'workspace', workspace_id: selectedWorkspace });
    pass('management PEP uses the same remote IAM protocol and trusted workspace');

    result = await req(base, 'POST', '/v1/config/brokered-models/refresh');
    assert.equal(result.status, 200, JSON.stringify(result.body));
    assert.equal(result.body.activated, 1);
    assert.deepEqual(
      iam.cloudCalls.map((entry) => entry.path),
      ['/v1/inference/readiness', '/v1/inference/models'],
    );
    assert.ok(iam.cloudCalls.every((entry) => entry.authorization === `Bearer ${cachedToken}`));
    result = await req(base, 'GET', '/v1/config/catalog');
    assert.equal(result.status, 200, JSON.stringify(result.body));
    const brokered = result.body.offerings.find((offering) => offering.source === 'brokered');
    assert.ok(brokered, JSON.stringify(result.body));
    assert.equal(brokered.model_id, 'gpt-5-e2e');
    assert.equal(result.body.model_attributes['gpt-5-e2e'].context_window, 400000);
    assert.equal(
      result.body.model_attributes['gpt-5-e2e'].provenance.context_window.source,
      'brokered',
    );

    result = await req(base, 'PUT', '/v1/config/inference-profiles/workspace-default', cachedToken, {
      body: {
        workspace_id: localWorkspace,
        primary: {
          target: {
            model_id: brokered.model_id,
            provider_id: brokered.provider_id,
            protocol_endpoint_id: brokered.protocol_endpoint_id,
          },
          credential_binding: { type: 'brokered' },
        },
        fallbacks: [],
        disabled_endpoint_ids: [],
      },
    });
    assert.equal(result.status, 200, JSON.stringify(result.body));
    result = await req(
      base,
      'POST',
      '/v1/config/inference-profiles/workspace-default/resolve-candidates',
      cachedToken,
      { body: { workspace_id: localWorkspace } },
    );
    assert.equal(result.status, 200, JSON.stringify(result.body));
    assert.equal(result.body.candidates[0].model_id, 'gpt-5-e2e');
    assert.equal(result.body.candidates[0].credential_present, false);
    pass('Cloud model refresh and explicit brokered Profile preserve exact public identity');

    iam.setCloudModels([]);
    result = await req(base, 'POST', '/v1/config/brokered-models/refresh');
    assert.equal(result.status, 200, JSON.stringify(result.body));
    assert.equal(result.body.marked_unavailable, 1);
    result = await req(base, 'GET', '/v1/config/catalog');
    assert.equal(
      result.body.offerings.find((offering) => offering.model_id === 'gpt-5-e2e').status,
      'unavailable',
    );
  assert.equal(result.body.model_attributes?.['gpt-5-e2e'], undefined);
    pass('on-demand refresh marks removed Cloud models unavailable and clears only Cloud metadata');

    // Cloud identity never exposes the self-managed API-token administration
    // surface, even when the remote PDP would otherwise allow it.
    const callsBeforeTokenAdmin = iam.calls.length;
    result = await req(base, 'GET', '/v1/config/iam/tokens', explicitToken);
    assert.equal(result.status, 404, 'self-managed token administration is not mounted in cloud mode');
    assert.equal(iam.calls.length, callsBeforeTokenAdmin, 'token administration does not reach PDP');

    for (const invalid of ['not-a-jwt', wrongAudienceToken]) {
      assert.equal((await req(base, 'GET', '/v1/files', invalid)).status, 401);
      assert.equal((await req(base, 'GET', '/v1/config/catalog', invalid)).status, 401);
    }
    for (const uri of ['/v1/files', '/v1/config/catalog']) {
      const expired = await req(base, 'GET', uri, expiredToken);
      assert.equal(expired.status, 401);
      assert.match(expired.body.error.message, /cloud access token is expired/u);
    }
    pass('malformed/wrong-audience tokens fail closed and expiry keeps its reason');

    iam.decide('deny');
    assert.equal((await req(base, 'GET', '/v1/files', explicitToken)).status, 403);
    assert.equal((await req(base, 'GET', '/v1/config/catalog', explicitToken)).status, 403);
    iam.decide('require_approval');
    assert.equal((await req(base, 'GET', '/v1/skills', explicitToken)).status, 403);
    assert.equal((await req(base, 'GET', '/v1/config/catalog', explicitToken)).status, 403);
    pass('remote deny and approval obligations remain fail-closed at both PEPs');

    console.log('E2E PASS: Awaken Cloud login and remote authorization stay outside resource services.');
  } finally {
    if (server) await stopServer(server);
    upstream.close();
    await iam.close();
    fs.rmSync(directory, { recursive: true, force: true });
  }
}

main().catch((error) => {
  console.error('E2E FAIL:', error);
  process.exitCode = 1;
});
