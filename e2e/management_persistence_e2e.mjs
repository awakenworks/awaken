// Cause graph (durable management restart):
//   C1 domain aggregate is authored before restart -> E1 durable row is restored
//   C2 secret is sealed with the same key          -> E2 the SDK-entered MCP credential materializes
//   C3 official Vault/Credential IDs are durable   -> E3 SDK reads survive restart
//   C4 restored Agent uses restored MCP binding    -> E4 authenticated tool call works
//   C5 session explicitly allows the MCP tool      -> E5 transport proof is not paused by HITL
//   C6 bootstrap identity and platform scope persist -> E6 every durable read/write stays authorized
//   C7 public Agent/Profile ids survive restart    -> E7 official SDK reads the same aggregates
//   C8 provider catalog survives restart           -> E8 SDK Models projects the restored supply
//   C9 Deployment/Run commit before restart         -> E9 both public SDK aggregates are restored
//
// Decision table:
//   Rule  C1  C2  C3  C4  C5  C6  Expected
//   T1    Y   -   -   -   -   Y   E1 + E6 (catalog/pool/normalized profile/Agent)
//   T2    Y   Y   -   Y   Y   Y   E2 + E4 + E5 + E6
//   T3    -   -   Y   -   -   Y   E3 + E6 (secret-free SDK projections)
//   T4    Y   -   Y   -   -   Y   E7 + E8 (public SDK projections after restart)
//   T5    Y   -   -   -   -   Y   E9 (Deployment/Run and linked Session survive)
//
// Restart-persistence e2e for the durable management plane (ADR-0043): spawn
// awaken-server in `management` mode with a fixed typed data_dir + seal key,
// author config through `/v1/config/*` AND enter an
// `mcp_oauth` credential through the official Anthropic SDK's vault front door
// (`beta.vaults.*`), kill the process, respawn it over the same dir/key, and
// assert exactly the documented persistence contract:
//
//   - DOMAIN state persists: catalog, secret-free credential rows (the SDK-entered
//     vault credential included), pool, inference profile, typed Agent MCP
//     binding — and an MCP conversation still works, i.e. the sealed
//     access token materialized from SQLite after the restart.
//   - SDK resource identity persists: Vault and Credential retrieve by the same
//     IDs after restart, while their projections remain secret-free.
//
// Run: (from e2e/)  npm install && node management_persistence_e2e.mjs

import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import Anthropic from '@anthropic-ai/sdk';
import {
  deploymentEnv,
  spawnServer,
  stopServer,
  waitForPort,
  waitForSessionEventReceipt,
  pass,
  startUpstream,
  realServerEnv,
  FAKE_KEY,
} from './harness.mjs';
import { startCalcFixture } from './fixtures/mcp_calc_fixture.mjs';

const BETAS = ['managed-agents-2026-04-01'];
const PORT = 38195;
const CALC_TOKEN = 'calc-persist-bearer-token'; // awaken-allow: secret
// 64 hex chars = the 32-byte AEAD key typed control_seal_key requires.
const SEAL_KEY = '00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff';

async function req(base, method, uri, body, token) {
  const headers = {};
  if (body !== undefined) headers['content-type'] = 'application/json';
  if (token !== undefined) headers.authorization = `Bearer ${token}`;
  const res = await fetch(`${base}${uri}`, {
    method,
    headers,
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  const text = await res.text();
  const json = text ? JSON.parse(text) : null;
  return { status: res.status, json };
}

function agentMessages(events) {
  return events
    .filter((event) => event.type === 'agent.message')
    .flatMap((event) => event.content ?? [])
    .filter((block) => block.type === 'text')
    .map((block) => block.text);
}

async function main() {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'awaken-mgmt-e2e-'));
  const env = deploymentEnv(dir, { identityMode: 'self-managed', controlSealKey: SEAL_KEY });
  const fixture = await startCalcFixture(CALC_TOKEN);
  const upstream = await startUpstream('mcp', { models: ['fake-haiku'] });
  let server = null;
  try {
    // ---- lifetime A: author everything ------------------------------------
    let { server: a, baseUrl: base } = spawnServer('management', PORT, { ...env, ...realServerEnv('mcp', upstream, { mode: 'management' }) });
    server = a;
    await waitForPort(PORT);
    const adminTokenPath = path.join(dir, 'admin-token');
    const adminToken = fs.readFileSync(adminTokenPath, 'utf8').trim();
    const workspace = fs.readFileSync(path.join(dir, 'platform-workspace-id'), 'utf8').trim();
    const request = (method, uri, body) => req(base, method, uri, body, adminToken);
    const client = new Anthropic({ apiKey: null, authToken: adminToken, baseURL: base });

    // The canonical connection command persists catalog and credential facts.
    let r = await request('POST', '/v1/config/provider-connections', {
      idempotency_key: 'management-persistence-provider-connection',
      workspace_id: workspace,
      provider_id: 'anthropic',
      display_name: 'Anthropic',
      dialect: 'anthropic_messages',
      base_url: `${upstream.url}/v1/`,
      timeout_secs: 300,
      secret: FAKE_KEY,
    });
    assert.equal(r.status, 201);
    pass('connected provider under the typed data_dir');

    // A vault `mcp_oauth` credential through the OFFICIAL SDK: the wire vault
    // resource identity, domain row, and sealed access token land in the
    // durable stores; no plaintext enters either read projection.
    const vault = await client.beta.vaults.create({ display_name: 'persist vault', betas: BETAS });
    const wireCred = await client.beta.vaults.credentials.create(vault.id, {
      type: 'mcp_oauth',
      mcp_server_url: fixture.url,
      access_token: CALC_TOKEN,
      betas: BETAS,
    });
    assert.equal(wireCred.auth.type, 'mcp_oauth');
    assert.ok(!JSON.stringify(wireCred).includes(CALC_TOKEN), 'wire credential is secret-free');

    const userProfile = await client.beta.userProfiles.create({
      external_id: 'management-persistence-profile',
      name: 'Persistent User Profile',
      relationship: 'external',
      metadata: { lifecycle: 'before-restart' },
    });
    assert.equal(userProfile.type, 'user_profile');

    // The domain row the SDK entry created. The wire vault id is only a container
    // id; the durable row is owned by the platform-resolved local workspace.
    r = await request('GET', `/v1/config/credentials?workspace_id=${workspace}`);
    assert.equal(r.status, 200);
    const mcpCredentials = r.json.filter((credential) => credential.descriptor?.targets?.some(
      ({ target }) => target?.purpose?.type === 'mcp_authorization',
    ));
    assert.equal(mcpCredentials.length, 1, `exactly one normalized MCP credential is present: ${JSON.stringify(r.json)}`);
    const [mcpCredential] = mcpCredentials;
    assert.equal(
      new URL(mcpCredential.descriptor.provider).origin,
      new URL(fixture.url).origin,
      'the descriptor retains the canonical MCP authority',
    );
    assert.equal(mcpCredential.descriptor.material.kind, 'secret');
    assert.equal(mcpCredential.descriptor.targets[0].target.purpose.type, 'mcp_authorization');
    assert.equal(mcpCredential.kind, 'vault', 'the secret remains backed by the canonical vault kind');
    const credId = mcpCredential.id;
    pass(`SDK vault mcp_oauth credential entered -> domain row ${credId} (secret-free)`);

    // Admin aggregates: pool + profile + one typed Agent definition whose MCP
    // binding freezes the exact SDK-entered credential revision.
    r = await request('PUT', '/v1/config/credential-pools/pool1', {
      id: 'pool1', workspace_id: workspace,
      members: [{ credential_source_id: credId, ordinal: 0, enabled: true, selection_weight: 0 }],
    });
    assert.equal(r.status, 200);
    r = await request('PUT', '/v1/config/inference-profiles/prof1', {
      primary: {
        target: { model_id: 'claude-opus-4-8' },
        credential_binding: { type: 'exact', credential_source_id: credId },
      },
      disabled_endpoint_ids: [],
    });
    assert.equal(r.status, 200);
    r = await request('PUT', '/v1/config/agents/calc-agent', {
      name: 'Calculator',
      system: 'Use the calculator tool and report its result.',
      model: {
        mode: 'pinned',
        provider_identity_ref: 'default',
        model_ref: 'fake-haiku',
        backend_ref: 'default',
      },
      mcp_servers: [{
        name: 'calc',
        url: fixture.url,
        credential: { id: credId, revision: 1 },
      }],
      tools: [{
        type: 'mcp_toolset',
        mcp_server_name: 'calc',
        default_config: {
          enabled: true,
          permission_policy: { type: 'always_allow' },
        },
      }],
    });
    assert.equal(r.status, 200);
    r = await request('POST', '/v1/config/agents/calc-agent/publish');
    assert.equal(r.status, 200, JSON.stringify(r.json));
    pass('authored pool + profile + published typed Agent MCP binding');

    // Test design: deployment_run_survives_process_replacement
    // Cause/effect graph: durable Agent + Environment -> Deployment commit ->
    // manual trigger -> DeploymentRun + linked Session commits -> process loss
    // -> a fresh SDK client reconstructs all four identities from durable facts.
    // Decision table:
    // | Deployment | Run | same data root | expected after restart |
    // | committed  | committed | yes      | retrieve/list exact IDs |
    // | committed  | absent    | yes      | Deployment only         |
    // | any        | any       | no       | no recovery claim       |
    // The test waits for the linked Session to become terminal before stopping
    // the process, separating durable recovery from in-flight crash recovery.
    const deploymentEnvironment = await client.beta.environments.create({
      name: 'management-persistence-deployment',
      config: { type: 'cloud' },
      betas: BETAS,
    });
    const deployment = await client.beta.deployments.create({
      agent: 'calc-agent',
      environment_id: deploymentEnvironment.id,
      name: 'management-persistence-deployment',
      initial_events: [{
        type: 'user.message',
        content: [{ type: 'text', text: 'Return a short persistence acknowledgement.' }],
      }],
      betas: BETAS,
    });
    const deploymentRun = await client.beta.deployments.run(deployment.id, { betas: BETAS });
    assert.equal(deploymentRun.deployment_id, deployment.id);
    assert.ok(deploymentRun.session_id, 'manual DeploymentRun links a Session before restart');
    const runDeadline = Date.now() + 60_000;
    let linkedSession;
    while (Date.now() < runDeadline) {
      linkedSession = await client.beta.sessions.retrieve(deploymentRun.session_id, { betas: BETAS });
      if (['idle', 'failed'].includes(linkedSession.status)) break;
      await new Promise((resolve) => setTimeout(resolve, 100));
    }
    assert.ok(
      linkedSession && ['idle', 'failed'].includes(linkedSession.status),
      `linked Session settles before restart: ${JSON.stringify(linkedSession)}`,
    );
    pass('official SDK committed Deployment/Run/Session recovery fixture');

    // ---- restart: kill the process, respawn over the same dir + key -------
    await stopServer(server);
    server = null;
    ({ server, baseUrl: base } = spawnServer('management', PORT, { ...env, ...realServerEnv('mcp', upstream, { mode: 'management' }) }));
    await waitForPort(PORT);
    assert.equal(fs.readFileSync(adminTokenPath, 'utf8').trim(), adminToken, 'admin identity persisted');
    const client2 = new Anthropic({ apiKey: null, authToken: adminToken, baseURL: base });
    pass('server killed and respawned on the same port with the same typed data_dir/key');

    const restoredVault = await client2.beta.vaults.retrieve(vault.id, { betas: BETAS });
    assert.equal(restoredVault.id, vault.id);
    const restoredCredential = await client2.beta.vaults.credentials.retrieve(wireCred.id, {
      vault_id: vault.id,
      betas: BETAS,
    });
    assert.equal(restoredCredential.id, wireCred.id);
    assert.ok(!JSON.stringify(restoredCredential).includes(CALC_TOKEN));
    pass('official SDK Vault/Credential identities persist across restart without secret echo');

    // Test design: public_management_projections_survive_process_replacement
    // Cause/effect graph: durable Config/Profile/Provider rows -> fresh process
    // composition -> Agent/UserProfile/Models protocol adapters -> official SDK
    // typed projections. No internal config GET is accepted as substitute
    // evidence for the public compatibility surface.
    // Decision table: same data root+seal key => stable ids/metadata/model;
    // empty cache or alternate store => missing SDK row and test failure.
    const restoredAgent = await client2.beta.agents.retrieve('calc-agent', { betas: BETAS });
    assert.equal(restoredAgent.id, 'calc-agent');
    assert.equal(restoredAgent.name, 'Calculator');
    assert.equal(restoredAgent.mcp_servers[0]?.url, fixture.url);
    const restoredProfile = await client2.beta.userProfiles.retrieve(userProfile.id);
    assert.equal(restoredProfile.id, userProfile.id);
    assert.equal(restoredProfile.external_id, 'management-persistence-profile');
    assert.equal(restoredProfile.metadata.lifecycle, 'before-restart');
    const restoredProfiles = [];
    for await (const profile of client2.beta.userProfiles.list()) restoredProfiles.push(profile.id);
    assert.ok(restoredProfiles.includes(userProfile.id));
    const restoredModels = [];
    for await (const model of client2.beta.models.list()) restoredModels.push(model.id);
    assert.ok(restoredModels.includes('fake-haiku'));
    pass('official SDK Agent/UserProfile/Models projections survive process replacement');

    const restoredDeploymentEnvironment = await client2.beta.environments.retrieve(
      deploymentEnvironment.id,
      { betas: BETAS },
    );
    assert.equal(restoredDeploymentEnvironment.id, deploymentEnvironment.id);
    const restoredDeployment = await client2.beta.deployments.retrieve(deployment.id, {
      betas: BETAS,
    });
    assert.equal(restoredDeployment.id, deployment.id);
    assert.equal(restoredDeployment.agent.id, 'calc-agent');
    const restoredRun = await client2.beta.deploymentRuns.retrieve(deploymentRun.id, {
      betas: BETAS,
    });
    assert.equal(restoredRun.id, deploymentRun.id);
    assert.equal(restoredRun.deployment_id, deployment.id);
    assert.equal(restoredRun.session_id, deploymentRun.session_id);
    const restoredRuns = [];
    for await (const run of client2.beta.deploymentRuns.list({
      deployment_id: deployment.id,
      betas: BETAS,
    })) restoredRuns.push(run.id);
    assert.ok(restoredRuns.includes(deploymentRun.id));
    assert.equal(
      (await client2.beta.sessions.retrieve(deploymentRun.session_id, { betas: BETAS })).id,
      deploymentRun.session_id,
    );
    pass('official SDK Environment/Deployment/DeploymentRun/Session identities survive restart');

    // The DOMAIN state persisted: every admin GET returns the authored object.
    r = await request('GET', '/v1/config/catalog');
    assert.equal(r.status, 200);
    assert.ok(
      'anthropic' in r.json.providers
        && 'anthropic.anthropic_messages' in r.json.endpoints,
    );

    r = await request('GET', `/v1/config/credentials?workspace_id=${workspace}`);
    assert.equal(r.status, 200);
    assert.ok(r.json.some((credential) => credential.id === credId));
    assert.ok(!JSON.stringify(r.json).includes(CALC_TOKEN), 'persisted rows stay secret-free');

    r = await request('GET', '/v1/config/credential-pools/pool1');
    assert.equal(r.status, 200);
    assert.equal(r.json.members.length, 1);

    r = await request('GET', '/v1/config/inference-profiles/prof1');
    assert.equal(r.status, 200);
    assert.equal(r.json.primary.target.model_id, 'claude-opus-4-8');
    assert.equal(r.json.primary.credential_binding.credential_source_id, credId);

    r = await request('GET', '/v1/config/agents/calc-agent');
    assert.equal(r.status, 200);
    assert.equal(r.json.mcp_servers[0].url, fixture.url);
    assert.deepEqual(r.json.mcp_servers[0].credential, { id: credId, revision: 1 });
    pass('catalog + credential + pool + profile + typed Agent binding all persisted');

    // ...and an MCP conversation works through the ADMIN-authored path (no
    // inline mcp_servers), with the PERSISTED credential as the bearer.
    const before = fixture.calls.filter((c) => c.method === 'tools/call').length;
    const session = await client2.beta.sessions.create({
      environment_id: 'env_local',
      agent: {
        id: 'calc-agent',
        type: 'agent_with_overrides',
        tools: [{
          type: 'mcp_toolset',
          mcp_server_name: 'calc',
          default_config: {
            enabled: true,
            permission_policy: { type: 'always_allow' },
          },
        }],
      },
      environment_id: 'env_local',
      betas: BETAS,
    });
    // P1: C1=restarted config/credential materialize; C2=exact User receipt;
    // C3=MCP result 15 commits. E1=post-restart conversation succeeds.
    // Constraint: only C3 after C2 qualifies. C1+C2&&!C3=>observe; all=>E1.
    const receipt = await client2.beta.sessions.events.send(session.id, {
      events: [{ type: 'user.message', content: [{ type: 'text', text: 'add 7 8' }] }],
      betas: BETAS,
    });
    const { delta: events } = await waitForSessionEventReceipt(
      client2,
      session.id,
      receipt.data[0]?.id,
      BETAS,
      ({ delta }) => agentMessages(delta).some((message) => message.includes('result: 15')),
      'P1 post-restart MCP result commits after its exact receipt',
    );
    assert.ok(
      events.some((e) => e.type === 'agent.mcp_tool_use' && e.name === 'mcp__calc__add'),
      `an mcp__calc__add tool_use: ${JSON.stringify(events.map((e) => e.type))}`,
    );
    const messages = agentMessages(events);
    assert.ok(
      messages.some((message) => message.includes('result: 15')),
      `the conversation reports result: 15 — messages=${JSON.stringify(messages)}, event types=${JSON.stringify(events.map((event) => event.type))}`,
    );
    const toolCalls = fixture.calls.filter((c) => c.method === 'tools/call');
    assert.equal(toolCalls.length, before + 1);
    assert.equal(toolCalls.at(-1).authorization, `Bearer ${CALC_TOKEN}`);
    assert.equal(fixture.unauthorized, 0, 'no request was ever rejected for missing auth');
    pass('post-restart MCP conversation works with the persisted sealed credential as bearer');

    console.log(
      'E2E PASS: authored config plus secret-free Vault/Credential resources and sealed material survive restart.',
    );
    process.exitCode = 0;
  } catch (err) {
    console.error('E2E FAIL:', err);
    process.exitCode = 1;
  } finally {
    if (server) await stopServer(server);
    await fixture.close();
    upstream.close();
    fs.rmSync(dir, { recursive: true, force: true });
  }
}

main();
