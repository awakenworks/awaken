// Embedded-IAM e2e for the management plane (ADR-0042/0043 P1): spawn
// awaken-server in `management` mode with AWAKEN_MGMT_DIR +
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
//     the authored config persisted;
//   - the HTTP token-management surface rotates the bootstrap credential:
//     POST /v1/config/iam/tokens mints a workspace admin token (cleartext
//     returned exactly once), the NEW token authors config, the list is
//     secret-free, DELETE /v1/config/iam/tokens/{id} revokes the bootstrap
//     token (old 401s, new keeps working), and a further restart persists
//     both facts.
//
// Every other e2e runs WITHOUT the AWAKEN_MGMT_IAM env var and stays open.
//
// Run: (from e2e/)  npm install && node management_authz_e2e.mjs

import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import Anthropic from '@anthropic-ai/sdk';
import { spawnServer, stopServer, waitForPort, pass, startUpstream, realServerEnv } from './harness.mjs';

const BETAS = ['managed-agents-2026-04-01'];
const PORT = 38197;
// 64 hex chars = the 32-byte AEAD key AWAKEN_MGMT_SEAL_KEY requires.
const SEAL_KEY = 'ffeeddccbbaa99887766554433221100ffeeddccbbaa99887766554433221100';

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
  return { status: res.status, json, text };
}

async function main() {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'awaken-mgmt-authz-e2e-'));
  const env = { AWAKEN_MGMT_DIR: dir, AWAKEN_MGMT_SEAL_KEY: SEAL_KEY, AWAKEN_MGMT_IAM: 'embedded' };
  const upstream = await startUpstream('mcp');
  let server = null;
  try {
    // ---- lifetime A ---------------------------------------------------------
    let { server: a, baseUrl: base } = spawnServer('management', PORT, { ...env, ...realServerEnv('mcp', upstream, { mode: 'management' }) });
    server = a;
    await waitForPort(PORT);
    // Scope is provisioned once by the platform and persisted beside the other
    // local control-plane state. Tests consume that authority; they never invent
    // or hard-code a workspace coordinate.
    const workspace = fs.readFileSync(path.join(dir, 'platform-workspace-id'), 'utf8').trim();
    assert.ok(workspace.startsWith('workspace_local_'), `platform workspace: ${workspace}`);

    // The bootstrap contract: the admin token is on disk, owner-only.
    const tokenPath = path.join(dir, 'admin-token');
    const token = fs.readFileSync(tokenPath, 'utf8').trim();
    assert.ok(token.startsWith('sk-awaken-'), `bootstrap token shape: ${token.slice(0, 12)}…`);
    assert.equal(fs.statSync(tokenPath).mode & 0o777, 0o600, 'admin-token is mode 0600');
    pass('bootstrap admin token written to <dir>/admin-token (0600), sk-awaken-… shape');

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
      workspace_id: workspace, kind: 'vault', provider_id: 'anthropic',
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
    ({ server, baseUrl: base } = spawnServer('management', PORT, { ...env, ...realServerEnv('mcp', upstream, { mode: 'management' }) }));
    await waitForPort(PORT);

    // Hydration, not re-bootstrap: the token file is byte-identical and the
    // SAME token still authenticates; the authored domain state persisted.
    assert.equal(fs.readFileSync(tokenPath, 'utf8').trim(), token, 'admin-token not re-minted');
    r = await req(base, 'GET', '/v1/config/catalog', undefined, token);
    assert.equal(r.status, 200, `post-restart catalog: ${JSON.stringify(r.json)}`);
    assert.ok(r.json.providers && r.json.providers.anthropic, 'provider persisted across restart');
    r = await req(base, 'GET', `/v1/config/credentials?workspace_id=${workspace}`, undefined, token);
    assert.equal(r.status, 200);
    assert.ok(r.json.some((c) => c.id === credId), 'credential row persisted across restart');
    r = await req(base, 'GET', '/v1/config/catalog');
    assert.equal(r.status, 401, 'the gate survives the restart too');
    pass('restart: same token authenticates (iam.sqlite hydration), config + gate persist');

    // ---- token management: mint, author, rotate the bootstrap credential ---
    // Mint a workspace admin token over HTTP with the bootstrap token. The
    // cleartext comes back exactly once, next to the secret-free view.
    r = await req(base, 'POST', '/v1/config/iam/tokens',
      { workspace_id: workspace, role: 'workspace_admin' }, token);
    assert.equal(r.status, 201, `token mint: ${JSON.stringify(r.json)}`);
    const opToken = r.json.token;
    assert.ok(opToken.startsWith('sk-awaken-'), 'minted cleartext is sk-awaken-… shaped');
    assert.equal(r.json.api_token.workspace_id, workspace);
    assert.equal(r.json.api_token.role, 'workspace_admin');
    assert.ok(!JSON.stringify(r.json.api_token).includes('$argon2'), 'view is hash-free');

    // The NEW token authors config within its role's reach.
    r = await req(base, 'PUT', '/v1/config/providers/openai',
      { id: 'openai', slug: 'openai', display_name: 'OpenAI', version: 1 }, opToken);
    assert.equal(r.status, 200, `new-token provider put: ${JSON.stringify(r.json)}`);
    pass('HTTP-minted workspace admin token authors config (cleartext returned once)');

    // The token list is secret-free: views only — never a hash or cleartext.
    r = await req(base, 'GET', `/v1/config/iam/tokens?workspace_id=${workspace}`, undefined, opToken);
    assert.equal(r.status, 200, `token list: ${r.text}`);
    assert.ok(!r.text.includes('$argon2'), 'token list has no argon2 hash');
    assert.ok(!r.text.includes(opToken), 'token list has no minted cleartext');
    assert.ok(!r.text.includes(token), 'token list has no bootstrap cleartext');
    const bootstrapView = r.json.find((t) => t.principal_id === 'mgmt-bootstrap');
    assert.ok(bootstrapView, 'bootstrap token is listed by its view');
    pass('GET /v1/config/iam/tokens is secret-free (prefix/principal/role views only)');

    // Rotate: revoke the bootstrap token WITH the new token. The old
    // credential 401s immediately; the successor keeps working.
    r = await req(base, 'DELETE', `/v1/config/iam/tokens/${bootstrapView.id}`, undefined, opToken);
    assert.equal(r.status, 200, `bootstrap revoke: ${JSON.stringify(r.json)}`);
    assert.ok(r.json.revoked_at, 'revoked view carries revoked_at');
    r = await req(base, 'GET', '/v1/config/catalog', undefined, token);
    assert.equal(r.status, 401, 'revoked bootstrap token is refused');
    assert.equal(r.json.error.type, 'authentication_error');
    r = await req(base, 'GET', '/v1/config/catalog', undefined, opToken);
    assert.equal(r.status, 200, 'the successor token keeps working');
    r = await req(base, 'GET', '/v1/files', undefined, token);
    assert.equal(r.status, 401, 'revoked bootstrap token is refused by the resource PEP too');
    r = await req(base, 'GET', '/v1/files', undefined, 'sk-ant-bogus.bogus');
    assert.equal(r.status, 401, 'invalid credentials fail closed at the resource PEP');
    r = await req(base, 'GET', '/v1/workspaces/not-the-token-workspace/files', undefined, opToken);
    assert.equal(r.status, 403, 'a resource path cannot select a workspace outside token scope');
    pass('bootstrap token revoked over HTTP: old 401s, minted successor still passes');

    // An expiring token: valid before its expiry, refused after (the expired
    // arm of authentication — distinct from revocation).
    const soon = new Date(Date.now() + 2000).toISOString().replace(/\.\d{3}Z$/, 'Z');
    r = await req(base, 'POST', '/v1/config/iam/tokens', {
      workspace_id: workspace, role: 'workspace_admin', expires_at: soon,
    }, opToken);
    assert.equal(r.status, 201, `short-lived mint: ${JSON.stringify(r.json)}`);
    const shortLived = r.json.token;
    r = await req(base, 'GET', '/v1/config/catalog', undefined, shortLived);
    assert.equal(r.status, 200, 'short-lived token works before expiry');
    await new Promise((resolve) => setTimeout(resolve, 3000));
    r = await req(base, 'GET', '/v1/config/catalog', undefined, shortLived);
    assert.equal(r.status, 401, 'expired token is refused');
    r = await req(base, 'GET', '/v1/files', undefined, shortLived);
    assert.equal(r.status, 401, 'expired token is refused by the resource PEP too');
    pass('expiring token: 200 before expiry, 401 after');

    // ---- second restart: rotation and mint both persisted -----------------
    await stopServer(server);
    server = null;
    ({ server, baseUrl: base } = spawnServer('management', PORT, { ...env, ...realServerEnv('mcp', upstream, { mode: 'management' }) }));
    await waitForPort(PORT);

    r = await req(base, 'GET', '/v1/config/catalog', undefined, token);
    assert.equal(r.status, 401, 'bootstrap revocation survives the restart');
    r = await req(base, 'GET', '/v1/config/catalog', undefined, opToken);
    assert.equal(r.status, 200, `minted token survives the restart: ${JSON.stringify(r.json)}`);
    assert.ok(r.json.providers && r.json.providers.openai, 'new-token-authored provider persisted');
    pass('second restart: revocation + minted token persisted (iam.sqlite rows)');

    
console.log('management_authz_e2e: all checks passed');
  } finally {
    if (server) await stopServer(server);
    upstream.close();
    fs.rmSync(dir, { recursive: true, force: true });
  }
}

main().catch((err) => {
  console.error(err);
  process.exit(1);
});
