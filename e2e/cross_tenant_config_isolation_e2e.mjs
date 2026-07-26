// Cross-tenant config-store isolation e2e (scenario #45).
//
// Cause graph:
//   C1 token addresses another Workspace path -> E1 reject 403 before repository access
//   C2 global-id aggregate belongs to A       -> E2 hide/reject from B with 404
//   C3 profile logical id exists only in A    -> E3 B reads 404
//   C4 B authors same profile logical id      -> E4 create independent B-owned profile
//   C5 scoped Agent id is already owned by A  -> E5 B write is a no-op; A remains intact
//
// Decision table:
//   Rule  C1  C2  C3  C4  C5  Expected
//   T1    Y   -   -   -   -   E1
//   T2    N   Y   -   -   -   E2
//   T3    N   N   Y   N   -   E3
//   T4    N   N   Y   Y   -   E4; A and B may both use `shared-profile`
//   T5    N   N   -   -   Y   E5
//
// The config authoring plane is tenant-fenced by an opaque scope_id (ADR-0051).
// This drives the guarantee end to end through IAM + workspace addressing + the
// scoped store. Two fences compose:
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
import { deploymentEnv, spawnServer, stopServer, waitForPort, pass } from './harness.mjs';

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
  // Platform topology is explicit: token minting never auto-registers a scope.
  const env = deploymentEnv(dir, {
    identityMode: 'self-managed',
    iamWorkspaces: [WS_A, WS_B],
    controlSealKey: SEAL_KEY,
  });
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

    // Workspace-owned admin aggregates keep ownership on their durable row. A
    // second owner sees 404 (not 403/existence disclosure), and same-id writes
    // cannot transfer ownership. This exercises the intrinsic aggregate fence,
    // not a process-local owner index.
    const credentialA = await req(base, 'POST', '/v1/config/credentials', tokenA, {
      workspace_id: WS_A,
      kind: 'vault',
      provider_id: null,
      env_key: null,
      secret: `xtenant-${randomBytes(12).toString('hex')}`,
    });
    assert.equal(credentialA.status, 201, `credential A: ${credentialA.text.slice(0, 200)}`);
    const credentialId = credentialA.json.id;
    const poolBody = {
      id: 'shared-pool',
      workspace_id: WS_A,
      members: [{ credential_source_id: credentialId, ordinal: 0, enabled: true, selection_weight: 0 }],
    };
    assert.equal((await req(base, 'PUT', '/v1/config/credential-pools/shared-pool', tokenA, poolBody)).status, 200);
    const profileBody = {
      workspace_id: WS_A,
      model_id: 'tenant-model',
      credential_binding: { type: 'exact', credential_source_id: credentialId },
      disabled_endpoint_ids: [],
    };
    assert.equal((await req(base, 'PUT', '/v1/config/inference-profiles/shared-profile', tokenA, profileBody)).status, 200);
    const agentMcpBody = {
      name: 'tenant MCP agent',
      system: 'Use the tenant MCP server.',
      model: { mode: 'auto' },
      mcp_servers: [{
        name: 'shared-mcp',
        url: 'https://mcp.example.invalid/',
        credential: { id: credentialId, revision: credentialA.json.version },
      }],
    };
    assert.equal((await req(base, 'PUT', '/v1/config/agents/shared-mcp-agent', tokenA, agentMcpBody)).status, 200);

    const hiddenFromB = [
      ['GET', `/v1/config/credentials/${encodeURIComponent(credentialId)}`, undefined, 404],
      ['GET', `/v1/config/credentials/${encodeURIComponent(credentialId)}/availability`, undefined, 404],
      ['POST', `/v1/config/credentials/${encodeURIComponent(credentialId)}/cooldown`, { kind: 'quota', retry_after_secs: 60 }, 404],
      ['GET', '/v1/config/credential-pools/shared-pool', undefined, 404],
      ['GET', '/v1/config/credential-pools/shared-pool/eligible', undefined, 404],
      ['GET', '/v1/config/inference-profiles/shared-profile', undefined, 404],
      ['POST', '/v1/config/inference-profiles/shared-profile/resolve', { workspace_id: WS_B }, 404],
      ['GET', '/v1/config/agents/shared-mcp-agent', undefined, 404],
    ];
    for (const [method, uri, body, expectedStatus] of hiddenFromB) {
      const hidden = await req(base, method, uri, tokenB, body);
      assert.equal(hidden.status, expectedStatus, `WS-B ${method} ${uri} is fenced: ${hidden.status}`);
    }
    const rejectedTakeovers = [
      ['PUT', '/v1/config/credential-pools/shared-pool', { ...poolBody, workspace_id: WS_B }],
    ];
    for (const [method, uri, body] of rejectedTakeovers) {
      const rejected = await req(base, method, uri, tokenB, body);
      assert.equal(rejected.status, 404, `WS-B cannot take over ${uri}: ${rejected.status}`);
    }
    const credentialB = await req(base, 'POST', '/v1/config/credentials', tokenB, {
      workspace_id: WS_B,
      kind: 'vault',
      provider_id: null,
      env_key: null,
      secret: `xtenant-${randomBytes(12).toString('hex')}`,
    });
    assert.equal(credentialB.status, 201, `credential B: ${credentialB.text.slice(0, 200)}`);
    const profileBodyB = {
      ...profileBody,
      workspace_id: WS_B,
      credential_binding: { type: 'exact', credential_source_id: credentialB.json.id },
    };
    const putProfileB = await req(
      base,
      'PUT',
      '/v1/config/inference-profiles/shared-profile',
      tokenB,
      profileBodyB,
    );
    assert.equal(putProfileB.status, 200, `T4 profile B: ${putProfileB.text.slice(0, 200)}`);
    const getProfileB = await req(base, 'GET', '/v1/config/inference-profiles/shared-profile', tokenB);
    assert.equal(getProfileB.status, 200, `T4 B reads its profile: ${getProfileB.text.slice(0, 200)}`);
    assert.equal(getProfileB.json.workspace_id, WS_B);
    assert.equal(getProfileB.json.primary.credential_binding.credential_source_id, credentialB.json.id);
    assert.equal(
      (await req(base, 'PUT', '/v1/config/agents/shared-mcp-agent', tokenB, agentMcpBody)).status,
      200,
      'same-id Agent write is an owner-protected no-op',
    );
    assert.equal(
      (await req(base, 'GET', '/v1/config/agents/shared-mcp-agent', tokenB)).status,
      404,
    );
    assert.equal((await req(base, 'GET', '/v1/config/credential-pools/shared-pool', tokenA)).json.workspace_id, WS_A);
    const getProfileA = await req(base, 'GET', '/v1/config/inference-profiles/shared-profile', tokenA);
    assert.equal(getProfileA.json.workspace_id, WS_A);
    assert.equal(getProfileA.json.primary.credential_binding.credential_source_id, credentialId);
    assert.deepEqual(
      (await req(base, 'GET', '/v1/config/agents/shared-mcp-agent', tokenA)).json.mcp_servers[0].credential,
      { id: credentialId, revision: credentialA.json.version },
    );
    pass('T2-T4: global aggregates reject takeovers while same-id profiles remain independently Workspace-owned');

    const ALPHA_STEPS = 7;
    const BETA_STEPS = 3;

    // WS-A authors the agent under its own scope (max_steps=7).
    const putA = await req(base, 'PUT', cfgPath(WS_A, `/${AGENT_ID}`), tokenA, draft(ALPHA_STEPS));
    assert.equal(putA.status, 200, `WS-A author: ${putA.text.slice(0, 200)}`);
    const getA = await req(base, 'GET', cfgPath(WS_A, `/${AGENT_ID}`), tokenA);
    assert.equal(getA.status, 200, 'WS-A reads back its own agent');
    assert.equal(getA.json.max_steps, ALPHA_STEPS, 'WS-A sees its own max_steps=7');
    const generation = getA.json.generation;
    const conditional = await req(base, 'PUT', cfgPath(WS_A, `/${AGENT_ID}`), tokenA, {
      ...draft(ALPHA_STEPS), generation,
    });
    assert.equal(conditional.status, 200, `current generation applies: ${conditional.text.slice(0, 200)}`);
    assert.ok(conditional.json.generation > generation, 'a conditional write advances the generation');
    const stale = await req(base, 'PUT', cfgPath(WS_A, `/${AGENT_ID}`), tokenA, {
      ...draft(ALPHA_STEPS), generation,
    });
    assert.equal(stale.status, 409, 'a stale generation fails closed');
    assert.equal(stale.json.current_revision, conditional.json.generation);
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
