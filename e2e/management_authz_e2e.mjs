// Embedded-IAM e2e for the management plane (ADR-0042/0043 P1): spawn
// awaken-server-local in `management` mode with AWAKEN_MGMT_DIR +
// AWAKEN_MGMT_SEAL_KEY + AWAKEN_MGMT_IAM=embedded, read the bootstrap admin
// token the server wrote to `<dir>/admin-token` (mode 0600), and assert the
// gate end to end over real HTTP:
//
//   - without a token every /v1/config/* and /v1/vaults/* call is 401 in the
//     Managed error envelope (`type: error`, `error.type: authentication_error`);
//   - with `Authorization: Bearer <admin token>` the full authoring flow works
//     (provider catalog, credential entry), and the OFFICIAL Anthropic SDK
//     (authToken → Bearer) drives the vault front door incl. an `mcp_oauth`
//     credential;
//   - after a restart over the same dir the SAME token still authenticates
//     (hydration from iam.sqlite — the admin-token file is not re-minted) and
//     the authored config persisted.
//
// Every other e2e runs WITHOUT the AWAKEN_MGMT_IAM env var and stays open.
//
// Run: (from e2e/)  npm install && node management_authz_e2e.mjs

import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import Anthropic from '@anthropic-ai/sdk';
import { spawnServer, stopServer, waitForPort, pass } from './harness.mjs';

const BETAS = ['managed-agents-2026-04-01'];
const PORT = 38197;
// 64 hex chars = the 32-byte AEAD key AWAKEN_MGMT_SEAL_KEY requires.
const SEAL_KEY = 'ffeeddccbbaa99887766554433221100ffeeddccbbaa99887766554433221100';
const WORKSPACE = 'wrkspc_default'; // the bootstrap admin token's workspace

async function req(base, method, uri, body, token) {
  const headers = {};
  if (body !== undefined) headers['content-type'] = 'application/json';
  if (token !== undefined) headers['authorization'] = `Bearer ${token}`;
  const res = await fetch(`${base}${uri}`, {
    method,
    headers,
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  const text = await res.text();
  const json = text ? JSON.parse(text) : null;
  return { status: res.status, json };
}

async function main() {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'awaken-mgmt-authz-e2e-'));
  const env = { AWAKEN_MGMT_DIR: dir, AWAKEN_MGMT_SEAL_KEY: SEAL_KEY, AWAKEN_MGMT_IAM: 'embedded' };
  let server = null;
  try {
    // ---- lifetime A ---------------------------------------------------------
    let { server: a, baseUrl: base } = spawnServer('management', PORT, env);
    server = a;
    await waitForPort(PORT);

    // The bootstrap contract: the admin token is on disk, owner-only.
    const tokenPath = path.join(dir, 'admin-token');
    const token = fs.readFileSync(tokenPath, 'utf8').trim();
    assert.ok(token.startsWith('sk-ant-'), `bootstrap token shape: ${token.slice(0, 10)}…`);
    assert.equal(fs.statSync(tokenPath).mode & 0o777, 0o600, 'admin-token is mode 0600');
    pass('bootstrap admin token written to <dir>/admin-token (0600), sk-ant-… shape');

    // Without a token: 401 in the Managed error envelope, on both surfaces.
    let r = await req(base, 'GET', '/v1/config/catalog');
    assert.equal(r.status, 401, `unauthenticated catalog read: ${JSON.stringify(r.json)}`);
    assert.equal(r.json.type, 'error');
    assert.equal(r.json.error.type, 'authentication_error');
    r = await req(base, 'POST', '/v1/vaults', { display_name: 'nope' });
    assert.equal(r.status, 401);
    r = await req(base, 'GET', '/v1/config/catalog', undefined, 'sk-ant-bogus.bogus');
    assert.equal(r.status, 401, 'garbage token is rejected');
    pass('missing/garbage tokens -> 401 authentication_error on config + vault surfaces');

    // With the admin token: the full authoring flow.
    r = await req(base, 'PUT', '/v1/config/providers/anthropic',
      { id: 'anthropic', slug: 'anthropic', display_name: 'Anthropic', version: 1 }, token);
    assert.equal(r.status, 200, `provider put: ${JSON.stringify(r.json)}`);
    r = await req(base, 'GET', '/v1/config/catalog', undefined, token);
    assert.equal(r.status, 200);
    assert.ok(r.json.providers && r.json.providers.anthropic, 'authored provider is in the catalog');
    r = await req(base, 'POST', '/v1/config/credentials', {
      workspace_id: WORKSPACE, kind: 'vault', provider_id: 'anthropic',
      env_key: 'ANTHROPIC_API_KEY',
      secret: 'sk-authz-e2e-secret', // awaken-allow: secret
    }, token);
    assert.equal(r.status, 201, `credential entry: ${JSON.stringify(r.json)}`);
    const credId = r.json.id;
    pass('admin token -> 200/201 across catalog + credential authoring');

    // The OFFICIAL SDK over the Bearer path (authToken): the vault front door,
    // including an mcp_oauth credential entered WITH the token.
    const client = new Anthropic({ apiKey: null, authToken: token, baseURL: base });
    const vault = await client.beta.vaults.create({ display_name: 'authz vault', betas: BETAS });
    assert.equal(vault.type, 'vault');
    const wireCred = await client.beta.vaults.credentials.create(vault.id, {
      type: 'mcp_oauth',
      mcp_server_url: 'http://127.0.0.1:9/mcp',
      access_token: 'authz-e2e-mcp-bearer', // awaken-allow: secret
      betas: BETAS,
    });
    assert.equal(wireCred.auth.type, 'mcp_oauth');
    assert.ok(!JSON.stringify(wireCred).includes('authz-e2e-mcp-bearer'), 'wire credential is secret-free');
    pass('official SDK (authToken -> Bearer) authored a vault + mcp_oauth credential');

    // ---- restart over the same dir -----------------------------------------
    await stopServer(server);
    server = null;
    ({ server, baseUrl: base } = spawnServer('management', PORT, env));
    await waitForPort(PORT);

    // Hydration, not re-bootstrap: the token file is byte-identical and the
    // SAME token still authenticates; the authored domain state persisted.
    assert.equal(fs.readFileSync(tokenPath, 'utf8').trim(), token, 'admin-token not re-minted');
    r = await req(base, 'GET', '/v1/config/catalog', undefined, token);
    assert.equal(r.status, 200, `post-restart catalog: ${JSON.stringify(r.json)}`);
    assert.ok(r.json.providers && r.json.providers.anthropic, 'provider persisted across restart');
    r = await req(base, 'GET', `/v1/config/credentials?workspace_id=${WORKSPACE}`, undefined, token);
    assert.equal(r.status, 200);
    assert.ok(r.json.some((c) => c.id === credId), 'credential row persisted across restart');
    r = await req(base, 'GET', '/v1/config/catalog');
    assert.equal(r.status, 401, 'the gate survives the restart too');
    pass('restart: same token authenticates (iam.sqlite hydration), config + gate persist');

    console.log('management_authz_e2e: all checks passed');
  } finally {
    if (server) await stopServer(server);
    fs.rmSync(dir, { recursive: true, force: true });
  }
}

main().catch((err) => {
  console.error(err);
  process.exit(1);
});
