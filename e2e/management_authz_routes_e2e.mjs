// Authorization coverage across the guarded /v1/config surface (embedded IAM):
// with the bootstrap admin token the guard maps every route -> action and
// authorizes (never 401/403), without a token every route is 401, and a token
// mint with an unknown role is 422. Exercises the action_for route table + the
// per-route authorize path. Deterministic, CI-safe.

import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { spawnServer, stopServer, waitForPort, pass } from './harness.mjs';

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
  return { status: res.status };
}

const ROUTES = [
  ['GET', '/v1/config/catalog'],
  ['GET', '/v1/config/providers/ghost'],
  ['GET', '/v1/config/endpoints/ghost'],
  ['GET', '/v1/config/credentials?workspace_id=wrkspc_default'],
  ['GET', '/v1/config/credential-pools/ghost'],
  ['GET', '/v1/config/inference-profiles/ghost'],
  ['GET', '/v1/config/mcp-servers/ghost'],
  ['GET', '/v1/config/mcp-servers'],
  ['GET', '/v1/config/projects'],
  ['GET', '/v1/config/projects/ghost'],
  ['GET', '/v1/config/projects/ghost/agents/x/mcp'],
];

async function main() {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'awaken-authz-routes-'));
  const env = { AWAKEN_MGMT_DIR: dir, AWAKEN_MGMT_SEAL_KEY: SEAL_KEY, AWAKEN_MGMT_IAM: 'embedded' };
  let server;
  try {
    ({ server } = spawnServer('management', PORT, env));
    await waitForPort(PORT);
    const base = `http://127.0.0.1:${PORT}`;
    const token = fs.readFileSync(path.join(dir, 'admin-token'), 'utf8').trim();

    for (const [m, uri] of ROUTES) {
      const r = await req(base, m, uri, token);
      assert.ok(r.status !== 401 && r.status !== 403, `admin authorized on ${m} ${uri} (got ${r.status})`);
    }
    pass('admin token authorized across every guarded config route (action_for arms)');

    for (const [m, uri] of ROUTES) {
      const r = await req(base, m, uri);
      assert.equal(r.status, 401, `${m} ${uri} without token -> 401 (got ${r.status})`);
    }
    pass('every guarded config route rejects a missing credential -> 401');

    // Token-admin edge: minting with an unknown role is rejected.
    const bad = await req(base, 'POST', '/v1/config/iam/tokens', token, {
      workspace_id: 'ws',
      role: 'not-a-real-role',
    });
    assert.ok([400, 422].includes(bad.status), `unknown role -> 4xx (got ${bad.status})`);
    pass('minting a token with an unknown role is rejected');

    console.log('E2E PASS: authz route->action mapping + fencing across the config surface.');
  } finally {
    if (server) await stopServer(server);
    fs.rmSync(dir, { recursive: true, force: true });
  }
  process.exitCode = 0;
}

main().catch((err) => {
  console.error('E2E FAIL:', err);
  process.exitCode = 1;
});
