// Awaken Cloud identity over the production awaken-iam remote protocol. The
// fixture is an IAM service boundary (JWKS + PDP), not an awaken test endpoint:
// the spawned product still performs real JWT verification, workspace routing,
// PEP enforcement, and remote authorization over HTTP.

import assert from 'node:assert/strict';
import { generateKeyPairSync, sign } from 'node:crypto';
import fs from 'node:fs';
import http from 'node:http';
import os from 'node:os';
import path from 'node:path';
import { spawnServer, stopServer, waitForPort, pass, startUpstream, realServerEnv } from './harness.mjs';

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
  let decision = 'allow';
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
    decide(value) { decision = value; },
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
    const env = {
      AWAKEN_DEPLOYMENT_DATA_DIR: directory,
      AWAKEN_CONTROL_SEAL_KEY: SEAL_KEY,
      AWAKEN_IDENTITY_MODE: 'awaken-cloud',
      AWAKEN_CLOUD_IAM_URL: iam.url,
      AWAKEN_CLOUD_IAM_ISSUER: ISSUER,
      AWAKEN_CLOUD_IAM_AUDIENCE: AUDIENCE,
      AWAKEN_CLOUD_ACCESS_TOKEN: cachedToken,
      AWAKEN_CLOUD_IAM_SERVICE_TOKEN: SERVICE_TOKEN,
    };
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

    // Cloud identity never exposes the self-managed API-token administration
    // surface, even when the remote PDP would otherwise allow it.
    const callsBeforeTokenAdmin = iam.calls.length;
    result = await req(base, 'GET', '/v1/config/iam/tokens', explicitToken);
    assert.equal(result.status, 404, 'self-managed token administration is not mounted in cloud mode');
    assert.equal(iam.calls.length, callsBeforeTokenAdmin, 'token administration does not reach PDP');

    for (const invalid of ['not-a-jwt', expiredToken, wrongAudienceToken]) {
      assert.equal((await req(base, 'GET', '/v1/files', invalid)).status, 401);
      assert.equal((await req(base, 'GET', '/v1/config/catalog', invalid)).status, 401);
    }
    pass('malformed, expired, and wrong-audience cloud credentials fail closed');

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
