// Management workspace-fence e2e (scenario #39): under embedded IAM, the
// `management_guard` fences EVERY place a request names a workspace — the path
// (`/v1/workspaces/{ws}/…` addressing), the query string (`?workspace_id=`), and a
// top-level JSON body field (`workspace_id`). Each must equal the token's own
// workspace, else the guard fails closed with 403 `permission_error` BEFORE the
// handler runs. `management_authz_routes_e2e` covers missing-token 401 +
// admin-authorized; this pins the cross-workspace fence the others leave open.
//
// Chain: request -> with_workspace_path_addressing (stamp RequestTenancy) ->
//        management_guard (authenticate -> path/query/body workspace fence ->
//        authorize) -> handler.
//
// Run: (from e2e/) node management_workspace_fence_e2e.mjs

import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { spawnServer, stopServer, waitForPort, pass } from './harness.mjs';

const PORT = Number(process.env.E2E_PORT ?? 38609);
const SEAL_KEY = 'ffeeddccbbaa99887766554433221100ffeeddccbbaa99887766554433221100';
const OWN = 'wrkspc_default'; // the bootstrap admin token's workspace
const EVIL = 'wrkspc_evil'; // a workspace the token has no authority over

async function req(base, method, uri, token, body) {
  const headers = {};
  if (token) headers['authorization'] = `Bearer ${token}`;
  if (body !== undefined) headers['content-type'] = 'application/json';
  const res = await fetch(`${base}${uri}`, {
    method,
    headers,
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  const text = await res.text();
  let json = null;
  try {
    json = text ? JSON.parse(text) : null;
  } catch {
    /* non-JSON */
  }
  return { status: res.status, json, text };
}

function assertForbidden(r, where) {
  assert.equal(r.status, 403, `${where}: cross-workspace naming -> 403 (got ${r.status}: ${r.text.slice(0, 200)})`);
  assert.equal(r.json?.type, 'error', `${where}: Managed error envelope`);
  assert.equal(r.json?.error?.type, 'permission_error', `${where}: permission_error (got ${r.json?.error?.type})`);
}

async function main() {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'awaken-ws-fence-'));
  const env = { AWAKEN_MGMT_DIR: dir, AWAKEN_MGMT_SEAL_KEY: SEAL_KEY, AWAKEN_MGMT_IAM: 'embedded' };
  const { server, baseUrl: base } = spawnServer('management', PORT, env);
  try {
    await waitForPort(PORT);
    const token = fs.readFileSync(path.join(dir, 'admin-token'), 'utf8').trim();
    assert.ok(token.startsWith('sk-awaken-'), 'bootstrap admin token present');

    // ---- Query-string fence -----------------------------------------------
    const okQuery = await req(base, 'GET', `/v1/config/credentials?workspace_id=${OWN}`, token);
    assert.ok(okQuery.status !== 401 && okQuery.status !== 403, `own-workspace query passes the fence (got ${okQuery.status})`);
    pass('query workspace_id == token workspace -> passes the fence');

    const evilQuery = await req(base, 'GET', `/v1/config/credentials?workspace_id=${EVIL}`, token);
    assertForbidden(evilQuery, 'query fence');
    pass('query workspace_id != token workspace -> 403 permission_error');

    // ---- Body fence -------------------------------------------------------
    const evilBody = await req(base, 'POST', '/v1/config/credentials', token, {
      workspace_id: EVIL,
      kind: 'vault',
      provider_id: 'anthropic',
      env_key: 'ANTHROPIC_API_KEY',
      secret: 'sk-fence-e2e', // awaken-allow: secret
    });
    assertForbidden(evilBody, 'body fence');
    // A matching body workspace passes the fence (whatever the handler then does,
    // it is NOT a 403 from the guard).
    const okBody = await req(base, 'POST', '/v1/config/credentials', token, {
      workspace_id: OWN,
      kind: 'vault',
      provider_id: 'anthropic',
      env_key: 'ANTHROPIC_API_KEY',
      secret: 'sk-fence-e2e', // awaken-allow: secret
    });
    assert.notEqual(okBody.status, 403, `own-workspace body passes the fence (got ${okBody.status})`);
    pass('body workspace_id fence: mismatch -> 403, match -> passes');

    // ---- Path-addressing fence --------------------------------------------
    const okPath = await req(base, 'GET', `/v1/workspaces/${OWN}/config/catalog`, token);
    assert.ok(okPath.status !== 401 && okPath.status !== 403, `own-workspace path passes the fence (got ${okPath.status})`);
    const evilPath = await req(base, 'GET', `/v1/workspaces/${EVIL}/config/catalog`, token);
    assertForbidden(evilPath, 'path fence');
    pass('path /v1/workspaces/{ws}/… fence: own -> passes, foreign -> 403');

    // ---- Unmapped-route fail-closed (#38): the guard runs BEFORE routing, so a
    //      path/method with no entry in the action table is 403, never a silent
    //      pass. `POST /v1/config/catalog` (catalog is GET-only) is unmapped. -----
    const unmapped = await req(base, 'POST', '/v1/config/catalog', token, {});
    assert.equal(unmapped.status, 403, `unmapped route -> 403 (got ${unmapped.status}: ${unmapped.text.slice(0, 200)})`);
    assert.equal(unmapped.json?.error?.type, 'permission_error', 'unmapped route -> permission_error');
    pass('unmapped route (no action mapped) -> 403 fail-closed');

    // ---- The fence is authorization, not authentication: the token is valid.
    const stillValid = await req(base, 'GET', '/v1/config/catalog', token);
    assert.equal(stillValid.status, 200, 'the same token still authenticates on its own workspace');
    pass('the fenced token remains valid on its own workspace (403 was authz, not authn)');
  } finally {
    await stopServer(server);
    fs.rmSync(dir, { recursive: true, force: true });
  }

  console.log('E2E PASS: management workspace fence (query/body/path workspace_id mismatch -> 403 permission_error).');
}

main().catch((err) => {
  console.error('E2E FAIL:', err);
  process.exitCode = 1;
});
