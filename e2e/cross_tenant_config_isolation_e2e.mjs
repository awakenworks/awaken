// Cross-tenant config-store isolation e2e (scenario #45): the config authoring plane
// is tenant-fenced by an opaque scope_id (ADR-0051). This drives the guarantee end
// to end through IAM + workspace addressing + the scoped store (the store's own unit
// test `a_write_cannot_clobber_another_scopes_agent` proves it in isolation; nothing
// proved it over the full HTTP stack). Two fences compose:
//   • management_guard path fence: a token may only address its OWN workspace path
//     (`/v1/workspaces/{ws}/…`) — a cross-workspace path is 403.
//   • scoped store owner-protection: agent ids are global (id is the PK); the scoped
//     upsert is `ON CONFLICT(id) DO UPDATE … WHERE scope_id = excluded.scope_id`, so
//     a SECOND tenant reusing an owner's id is a deliberate NO-OP (returns 200 but
//     neither clobbers nor exposes the owner's row) and reads back 404 in its scope.
// The discriminator is `max_steps` (echoed in the GET projection; a draft's raw
// `instructions` projects to `system` only once published).
//
// Run: (from e2e/) node cross_tenant_config_isolation_e2e.mjs

import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { randomBytes } from 'node:crypto';
import { spawnServer, stopServer, waitForPort, pass } from './harness.mjs';

const PORT = Number(process.env.E2E_PORT ?? 38612);
const SEAL_KEY = 'ffeeddccbbaa99887766554433221100ffeeddccbbaa99887766554433221100';
const WS_A = 'wrkspc_alpha';
const WS_B = 'wrkspc_beta';
const AGENT_ID = 'shared-agent';

async function req(base, method, uri, token, body) {
  const headers = {};
  if (token) headers['authorization'] = `Bearer ${token}`;
  if (body !== undefined) headers['content-type'] = 'application/json';
  const res = await fetch(`${base}${uri}`, { method, headers, body: body === undefined ? undefined : JSON.stringify(body) });
  const text = await res.text();
  let json = null;
  try {
    json = text ? JSON.parse(text) : null;
  } catch {
    /* non-JSON */
  }
  return { status: res.status, json, text };
}

// `max_steps` is the per-scope discriminator: it survives into the GET projection.
const draft = (maxSteps) => ({
  id: AGENT_ID,
  instructions: 'a tenant-scoped agent',
  max_steps: maxSteps,
  model_binding: { mode: 'auto' },
  tool_ids: [],
  plugin_ids: [],
  plugin_config: {},
});

// Author under a workspace-addressed path (stamps WorkspaceScope = ws).
const cfgPath = (ws, tail) => `/v1/workspaces/${ws}/config/agents${tail}`;

async function main() {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'awaken-xtenant-'));
  const env = {
    AWAKEN_MGMT_DIR: dir,
    AWAKEN_MGMT_SEAL_KEY: SEAL_KEY,
    AWAKEN_MGMT_IAM: 'embedded',
    // Platform topology is explicit: token minting never auto-registers a scope.
    AWAKEN_IAM_WORKSPACES: `${WS_A},${WS_B}`,
  };
  const { server, baseUrl: base } = spawnServer('management', PORT, env);
  try {
    await waitForPort(PORT);
    const bootstrap = fs.readFileSync(path.join(dir, 'admin-token'), 'utf8').trim();

    // Mint a workspace-admin token for each platform-registered workspace. The
    // hidden-org bootstrap binding can administer both, but cannot invent scopes.
    const mintA = await req(base, 'POST', '/v1/config/iam/tokens', bootstrap, { workspace_id: WS_A, role: 'workspace_admin' });
    assert.equal(mintA.status, 201, `mint token A: ${mintA.text.slice(0, 200)}`);
    const tokenA = mintA.json.token;
    const mintB = await req(base, 'POST', '/v1/config/iam/tokens', bootstrap, { workspace_id: WS_B, role: 'workspace_admin' });
    assert.equal(mintB.status, 201, `mint token B: ${mintB.text.slice(0, 200)}`);
    const tokenB = mintB.json.token;
    pass('minted a workspace-admin token for each of two workspaces');

    const ALPHA_STEPS = 7;
    const BETA_STEPS = 3;

    // WS-A authors the agent under its own scope (max_steps=7).
    const putA = await req(base, 'PUT', cfgPath(WS_A, `/${AGENT_ID}`), tokenA, draft(ALPHA_STEPS));
    assert.equal(putA.status, 200, `WS-A author: ${putA.text.slice(0, 200)}`);
    const getA = await req(base, 'GET', cfgPath(WS_A, `/${AGENT_ID}`), tokenA);
    assert.equal(getA.status, 200, 'WS-A reads back its own agent');
    assert.equal(getA.json.max_steps, ALPHA_STEPS, 'WS-A sees its own max_steps=7');
    pass('WS-A authored + read its agent config in its own scope (max_steps=7)');

    // Fence: WS-A's token may not even ADDRESS another workspace's path -> 403.
    const crossPath = await req(base, 'GET', cfgPath(WS_B, `/${AGENT_ID}`), tokenA);
    assert.equal(crossPath.status, 403, `a token cannot address another workspace path (got ${crossPath.status})`);
    pass('management_guard: a token cannot address another workspace path -> 403');

    // Within WS-B's OWN scope, the id authored by WS-A does not exist -> 404.
    const missB = await req(base, 'GET', cfgPath(WS_B, `/${AGENT_ID}`), tokenB);
    assert.equal(missB.status, 404, `id absent from WS-B's scope -> 404 (got ${missB.status}: ${missB.text.slice(0, 200)})`);
    const listB = await req(base, 'GET', cfgPath(WS_B, ''), tokenB);
    assert.equal(listB.status, 200, 'WS-B list ok');
    const listBIds = (listB.json?.data ?? []).map((a) => a.id);
    assert.ok(!listBIds.includes(AGENT_ID), "WS-B's list does not contain WS-A's agent");
    pass('WS-B scope is empty of WS-A agent: 404-on-miss + list omits it');

    // WS-B tries to author the SAME id (owned by WS-A). Owner-protection: the write
    // is a deliberate no-op (200), so WS-B still cannot read it and WS-A's row is
    // untouched — a tenant can neither clobber nor hijack an owner's agent id.
    const putB = await req(base, 'PUT', cfgPath(WS_B, `/${AGENT_ID}`), tokenB, draft(BETA_STEPS));
    assert.equal(putB.status, 200, `WS-B same-id write returns 200 (no-op): ${putB.text.slice(0, 200)}`);
    const getBOwn = await req(base, 'GET', cfgPath(WS_B, `/${AGENT_ID}`), tokenB);
    assert.equal(getBOwn.status, 404, `the no-op did not create a WS-B row (still 404): ${getBOwn.text.slice(0, 120)}`);
    pass('WS-B same-id write is a no-op (200) that creates nothing in WS-B (owner-protected)');

    const getAAgain = await req(base, 'GET', cfgPath(WS_A, `/${AGENT_ID}`), tokenA);
    assert.equal(getAAgain.status, 200, 'WS-A agent still present after WS-B same-id write');
    assert.equal(getAAgain.json.max_steps, ALPHA_STEPS, "WS-A row unchanged (WS-B could not clobber the owner's config)");
    pass('WS-B could not clobber WS-A: the owner row is intact (max_steps still 7)');
  } finally {
    await stopServer(server);
    fs.rmSync(dir, { recursive: true, force: true });
  }

  console.log('E2E PASS: cross-tenant config-store isolation (per-scope rows, 404 cross-read, same-id isolation).');
}

main().catch((err) => {
  console.error('E2E FAIL:', err);
  process.exitCode = 1;
});
