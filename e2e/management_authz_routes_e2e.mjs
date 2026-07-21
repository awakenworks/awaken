// Authorization coverage across the guarded /v1/config surface (embedded IAM):
// with the bootstrap admin token the guard maps every route -> action and
// authorizes (never 401/403), without a token every route is 401, and a token
// mint with an unknown role is 422. Exercises the action_for route table + the
// per-route authorize path. Deterministic, CI-safe.

import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { spawnServer, stopServer, waitForPort, pass, startUpstream, realServerEnv } from './harness.mjs';

const PORT = 38253;
const SEAL_KEY = 'ffeeddccbbaa99887766554433221100ffeeddccbbaa99887766554433221100';

async function req(base, method, uri, token, body) {
  const headers = {};
  if (token) headers.authorization = `Bearer ${token}`;
  if (body !== undefined) headers['content-type'] = 'application/json';
  const res = await fetch(`${base}${uri}`, {
    method,
    headers,
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  const text = await res.text();
  return { status: res.status, body: text ? JSON.parse(text) : null };
}

async function apiKeyReq(base, method, uri, token, body) {
  const headers = { 'x-api-key': token };
  if (body !== undefined) headers['content-type'] = 'application/json';
  const res = await fetch(`${base}${uri}`, {
    method,
    headers,
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  return { status: res.status };
}

function routes(workspace) {
  return [
  ['GET', '/v1/config/catalog'],
  ['GET', '/v1/config/providers/ghost'],
  ['GET', '/v1/config/endpoints/ghost'],
  ['GET', `/v1/config/credentials?workspace_id=${encodeURIComponent(workspace)}`],
  ['GET', '/v1/config/credential-pools/ghost'],
  ['GET', '/v1/config/inference-profiles/ghost'],
  ['GET', '/v1/config/mcp-servers/ghost'],
  ['GET', '/v1/config/mcp-servers'],
  ];
}

async function main() {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'awaken-authz-routes-'));
  const env = { AWAKEN_MGMT_DIR: dir, AWAKEN_MGMT_SEAL_KEY: SEAL_KEY, AWAKEN_MGMT_IAM: 'embedded' };
  const upstream = await startUpstream('mcp');
  let server;
  try {
    ({ server } = spawnServer('management', PORT, { ...env, ...realServerEnv('mcp', upstream, { mode: 'management' }) }));
    await waitForPort(PORT);
    const base = `http://127.0.0.1:${PORT}`;
    const token = fs.readFileSync(path.join(dir, 'admin-token'), 'utf8').trim();
    const workspace = fs.readFileSync(path.join(dir, 'platform-workspace-id'), 'utf8').trim();
    const guardedRoutes = routes(workspace);

    for (const [m, uri] of guardedRoutes) {
      const r = await req(base, m, uri, token);
      assert.ok(r.status !== 401 && r.status !== 403, `admin authorized on ${m} ${uri} (got ${r.status})`);
    }
    pass('admin token authorized across every guarded config route (action_for arms)');

    for (const [m, uri] of guardedRoutes) {
      const r = await req(base, m, uri);
      assert.equal(r.status, 401, `${m} ${uri} without token -> 401 (got ${r.status})`);
    }
    pass('every guarded config route rejects a missing credential -> 401');

    // The resource PEP is a sibling of the Resource Catalog/stores. It maps the
    // three resource families to centrally governed actions, asks IAM, and stamps
    // only the trusted Workspace into the inner request.
    const resourceReads = ['/v1/files', '/v1/skills', '/v1/memory_stores'];
    for (const uri of resourceReads) {
      assert.equal((await req(base, 'GET', uri)).status, 401, `${uri} missing token -> 401`);
      const allowed = await req(base, 'GET', uri, token);
      assert.ok(
        allowed.status !== 401 && allowed.status !== 403,
        `admin token reads ${uri} (got ${allowed.status})`,
      );
    }
    const memory = await req(base, 'POST', '/v1/memory_stores', token, { name: 'authz-memory' });
    assert.ok(memory.status !== 401 && memory.status !== 403, `admin writes MemoryStore: ${memory.status}`);
    const memoryConfigRoute = `/v1/memory_stores/${memory.body.id}/config`;
    assert.equal((await req(base, 'GET', memoryConfigRoute)).status, 401);
    assert.equal(
      (
        await req(base, 'POST', memoryConfigRoute, token, {
          expected_config_version: 1,
          recall_policy: { enabled: true, max_results: 5 },
        })
      ).status,
      200,
    );
    const skill = await req(base, 'POST', '/v1/skills', token, {
      id: 'authz-skill',
      content: '---\nname: authz-skill\ndescription: authz\n---\nUse safely.',
    });
    assert.ok(skill.status !== 401 && skill.status !== 403, `admin writes Skill: ${skill.status}`);
    pass('resource PEP maps File/Skill/Memory reads and admin writes outside the stores');

    const restrictedMint = await fetch(`${base}/v1/config/iam/tokens`, {
      method: 'POST',
      headers: { authorization: `Bearer ${token}`, 'content-type': 'application/json' },
      body: JSON.stringify({
        workspace_id: workspace,
        role: 'workspace_restricted_developer',
      }),
    });
    assert.equal(restrictedMint.status, 201);
    const restricted = (await restrictedMint.json()).token;
    for (const uri of resourceReads) {
      const read = await req(base, 'GET', uri, restricted);
      assert.ok(read.status !== 401 && read.status !== 403, `read-only role reads ${uri}`);
    }
    assert.equal(
      (await req(base, 'POST', '/v1/memory_stores', restricted, { name: 'denied' })).status,
      403,
      'read-only role cannot create a MemoryStore',
    );
    assert.equal((await req(base, 'GET', memoryConfigRoute, restricted)).status, 200);
    assert.equal(
      (
        await req(base, 'POST', memoryConfigRoute, restricted, {
          expected_config_version: 2,
          extraction_policy: { enabled: false },
        })
      ).status,
      403,
      'read-only role cannot publish resource behavior configuration',
    );
    assert.ok(
      (await apiKeyReq(base, 'GET', '/v1/files', restricted)).status < 400,
      'x-api-key reaches the same resource PEP as Bearer',
    );
    pass('read-only resource authority is enforced at PEP for Bearer and x-api-key');

    // Token-admin edge: minting with an unknown role is rejected.
    const bad = await req(base, 'POST', '/v1/config/iam/tokens', token, {
      workspace_id: workspace,
      role: 'not-a-real-role',
    });
    assert.ok([400, 422].includes(bad.status), `unknown role -> 4xx (got ${bad.status})`);
    pass('minting a token with an unknown role is rejected');

    console.log('E2E PASS: authz route->action mapping + fencing across the config surface.');
  } finally {
    if (server) await stopServer(server);
    upstream.close();
    fs.rmSync(dir, { recursive: true, force: true });
  }
  process.exitCode = 0;
}

main().catch((err) => {
  console.error('E2E FAIL:', err);
  process.exitCode = 1;
});
