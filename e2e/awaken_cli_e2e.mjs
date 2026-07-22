// End-to-end for the aggregated `awaken` command (crate awaken-cli), Serve role.
//
// `awaken` is the single binary that subsumes awaken-server:
// configuration (AWAKEN_ROLE + the deployment axes) decides the deployment. The
// default Serve role mounts the production management assembly, whose
// host resolves each session's model from the **database-configured** catalog +
// credential vault (CredentialInferenceMaterializer) — not a baked-in demo model.
//
// This test proves that path through the real binary: author a provider / endpoint /
// offering + an Anthropic credential through the console API, publish an agent bound
// to that model, then run a session and assert the reply came from the configured
// model over the wire (a fake Anthropic upstream). It is the first coverage of the
// console-config → resolve → run chain end to end.
//
// Run: (from e2e/)  npm install && node awaken_cli_e2e.mjs

import assert from 'node:assert/strict';
import fs from 'node:fs';
import net from 'node:net';
import os from 'node:os';
import readline from 'node:readline';
import { spawn, execFileSync, execSync } from 'node:child_process';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import Anthropic from '@anthropic-ai/sdk';
import { startFakeAnthropic } from './fixtures/fake_anthropic_fixture.mjs';

const REPO_ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const PORT = Number(process.env.E2E_PORT ?? 38411);
const BETAS = ['managed-agents-2026-04-01'];
const FAKE_KEY = 'sk-awaken-cli-fake-key'; // awaken-allow: secret
// The composition root receives the platform-owned coordinate explicitly; no test
// or resource adapter relies on a compiled Workspace id.
const WORKSPACE = `workspace_e2e_${process.pid}`;
const AGENT = 'db-model-agent';
const MODEL = 'fake-haiku';
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

function awakenBin() {
  const out = execSync(
    'cargo build --quiet --message-format=json -p awaken-cli --bin awaken',
    { cwd: REPO_ROOT, maxBuffer: 64 * 1024 * 1024 },
  ).toString();
  for (const line of out.split('\n')) {
    if (!line.trim()) continue;
    let msg;
    try {
      msg = JSON.parse(line);
    } catch {
      continue;
    }
    if (msg.executable && msg.target?.name === 'awaken') return msg.executable;
  }
  throw new Error('could not resolve the awaken binary path');
}

function waitForPort(port, timeoutMs = 60_000) {
  const deadline = Date.now() + timeoutMs;
  return new Promise((resolve, reject) => {
    const attempt = () => {
      const sock = net.createConnection({ port, host: '127.0.0.1' });
      sock.once('connect', () => {
        sock.destroy();
        resolve();
      });
      sock.once('error', () => {
        sock.destroy();
        if (Date.now() > deadline) reject(new Error(`server did not listen on ${port}`));
        else setTimeout(attempt, 200);
      });
    };
    attempt();
  });
}

function startAwaken(bin, port, extraEnv = {}) {
  const server = spawn(bin, {
    env: { ...process.env, AWAKEN_HTTP_ADDR: `127.0.0.1:${port}`, ...extraEnv },
    stdio: ['ignore', 'inherit', 'pipe'],
  });
  readline.createInterface({ input: server.stderr }).on('line', (line) => {
    process.stderr.write(`${line}\n`);
  });
  const stop = () =>
    new Promise((resolve) => {
      if (server.exitCode !== null) return resolve();
      server.on('exit', () => resolve());
      server.kill('SIGINT');
    });
  return { server, baseUrl: `http://127.0.0.1:${port}`, stop };
}

async function req(base, method, uri, body) {
  const res = await fetch(`${base}${uri}`, {
    method,
    headers: body === undefined ? {} : { 'content-type': 'application/json' },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  const text = await res.text();
  let json = null;
  try {
    json = text ? JSON.parse(text) : null;
  } catch {
    json = { _raw: text };
  }
  return { status: res.status, json };
}

async function ready(base, timeoutMs = 60_000) {
  const deadline = Date.now() + timeoutMs;
  for (;;) {
    try {
      const res = await fetch(`${base}/v1/capabilities`);
      if (res.ok) return;
    } catch {
      /* not up yet */
    }
    if (Date.now() > deadline) throw new Error('management plane did not become ready');
    await sleep(200);
  }
}

function seedRepository(root) {
  const work = path.join(root, 'repository-work');
  const remote = path.join(root, 'repository.git');
  fs.mkdirSync(work, { recursive: true });
  execFileSync('git', ['init', '-q', '-b', 'main'], { cwd: work });
  execFileSync('git', ['config', 'user.email', 'resource-e2e@example.invalid'], { cwd: work });
  execFileSync('git', ['config', 'user.name', 'resource-e2e'], { cwd: work });
  fs.writeFileSync(path.join(work, 'README.md'), 'sqlite resource catalog');
  execFileSync('git', ['add', 'README.md'], { cwd: work });
  execFileSync('git', ['commit', '-q', '-m', 'seed'], { cwd: work });
  execFileSync('git', ['clone', '-q', '--bare', work, remote]);
  return remote;
}

async function main() {
  const upstream = await startFakeAnthropic(FAKE_KEY);
  const bin = awakenBin();
  const mgmtDir = fs.mkdtempSync(path.join(os.tmpdir(), 'awaken-cli-e2e-'));
  const serverEnv = {
    AWAKEN_LOCAL_WORKSPACE_ID: WORKSPACE,
    AWAKEN_MGMT_DIR: mgmtDir,
    AWAKEN_MGMT_SEAL_KEY: '00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff',
  };
  let h = startAwaken(bin, PORT, serverEnv);
  try {
    await waitForPort(PORT);
    await ready(h.baseUrl);
    console.log('ok: aggregated `awaken` command booted in the default Serve role (management plane)');

    // ---- author the model in the database-backed console ---------------------
    let base = h.baseUrl;
    let r = await req(base, 'PUT', '/v1/config/providers/anthropic', {
      id: 'anthropic', slug: 'anthropic', display_name: 'Anthropic', version: 1,
    });
    assert.equal(r.status, 200, `provider: ${JSON.stringify(r.json)}`);
    r = await req(base, 'PUT', '/v1/config/endpoints/ep1', {
      id: 'ep1', provider_id: 'anthropic', dialect: 'anthropic_messages',
      base_url: `${upstream.url}/v1/`, timeout_secs: 300, display_name: 'fake', version: 1,
    });
    assert.equal(r.status, 200, `endpoint: ${JSON.stringify(r.json)}`);
    r = await req(base, 'POST', '/v1/config/offerings', {
      model_id: MODEL, provider_id: 'anthropic',
      protocol_endpoint_id: 'ep1', dialect: 'anthropic_messages', upstream_model: null,
    });
    assert.equal(r.status, 200, `offering: ${JSON.stringify(r.json)}`);
    r = await req(base, 'PUT', `/v1/config/model-attributes/${MODEL}`, {
      context_window: 4096,
      max_output_tokens: 1024,
    });
    assert.equal(r.status, 200, `model attributes: ${JSON.stringify(r.json)}`);
    r = await req(base, 'POST', '/v1/config/credentials', {
      workspace_id: WORKSPACE, kind: 'vault', provider_id: 'anthropic',
      env_key: 'ANTHROPIC_API_KEY', secret: FAKE_KEY,
    });
    assert.equal(r.status, 201, `credential: ${JSON.stringify(r.json)}`);
    const credentialId = r.json.id;
    console.log('ok: authored provider/endpoint/offering + credential in the console DB');

    // ---- publish an agent bound to that model --------------------------------
    // The console agent object is the managed `/v1/agents` shape: the model is
    // `model: { id }`, which the config plane maps to a Pinned selection — so a
    // session for this agent runs exactly the DB-configured `MODEL`.
    r = await req(base, 'PUT', `/v1/config/agents/${AGENT}`, {
      name: AGENT,
      model: { id: MODEL },
      system: 'You are a test agent.',
      max_steps: 2,
      plugins: ['compact'],
      plugin_config: { compact: {}, acp: {} },
      compaction: { keep_recent: 2 },
    });
    assert.equal(r.status, 200, `agent config: ${JSON.stringify(r.json)}`);

    // Resource bindings are resolved by the configuration plane exactly once.
    // A negative resource revision fails publication; replacing it with a valid
    // revision contributes the prompt and typed input pin to the snapshot.
    const resources = (revision) => ({
      agent_id: AGENT,
      revision,
      inputs: [],
    });
    r = await req(base, 'PUT', `/v1/config/agents/${AGENT}/resources`, resources(-1));
    assert.equal(r.status, 422, `negative resource revision rejected at authoring: ${JSON.stringify(r.json)}`);
    assert.equal(r.json.code, 'invalid_revision');

    // Upgrade boundary: old `resources/version` rows are accepted once and
    // normalized into the canonical typed input language. File access narrows to
    // read-only; mutable Memory/Repository bindings retain authored access. Removed
    // output/Skill axes fail closed instead of entering the input union.
    const legacyAgent = 'legacy-input-agent';
    r = await req(base, 'PUT', `/v1/config/agents/${legacyAgent}/resources`, {
      agent_id: 'forged-path-id',
      version: 1,
      resources: [
        {
          kind: 'file', resource_id: 'legacy-file', mount_path: '/workspace/file',
          access: 'read_write', instructions: 'immutable input',
        },
        {
          kind: 'memory_store', resource_id: 'legacy-memory', mount_path: '/workspace/memory',
          access: 'read_write',
        },
        {
          kind: 'github_repository', resource_id: 'legacy-repository',
          mount_path: '/workspace/repository', access: 'read_only',
        },
      ],
    });
    assert.equal(r.status, 200, JSON.stringify(r.json));
    assert.equal(r.json.agent_id, legacyAgent, 'path Agent id is authoritative');
    assert.equal(r.json.revision, 1);
    assert.deepEqual(
      r.json.inputs.map((input) => input.target.kind),
      ['file', 'memory_store', 'repository'],
    );
    assert.equal(r.json.inputs[0].access, 'read_only');
    assert.equal(r.json.inputs[1].access, 'read_write');
    assert.equal(r.json.inputs[2].access, 'read_only');
    for (const removedKind of ['outputs', 'skill']) {
      r = await req(base, 'PUT', `/v1/config/agents/${legacyAgent}/resources`, {
        agent_id: legacyAgent,
        version: 2,
        resources: [{
          kind: removedKind,
          resource_id: 'removed-axis',
          mount_path: '/workspace/removed',
          access: 'read_only',
        }],
      });
      assert.equal(r.status, 422, `${removedKind} legacy binding fails closed: ${JSON.stringify(r.json)}`);
    }
    r = await req(base, 'PUT', `/v1/config/agents/${AGENT}/resources`, resources(1));
    assert.equal(r.status, 200, `valid resource revision staged: ${JSON.stringify(r.json)}`);
    r = await req(base, 'POST', `/v1/config/agents/${AGENT}/publish`, undefined);
    assert.equal(r.status, 200, `publish: ${JSON.stringify(r.json)}`);
    assert.equal(r.json.installed, true, 'published agent installed into the live catalog');
    console.log(`ok: published agent bound to the DB-configured model '${MODEL}'`);

    // The official Managed Agent API is an adapter over that same ConfigPlane,
    // not a process-local registry. Create + update through the SDK before the
    // restart; after restart its current revision, complete history and executable
    // snapshot must all come back from config.db.
    let client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: base });
    const managedAgent = await client.beta.agents.create({
      name: 'sdk-authored-agent',
      model: MODEL,
      system: 'You are authored through the SDK.',
      metadata: { source: 'managed-api' },
      betas: BETAS,
    });
    const managedUpdated = await client.beta.agents.update(managedAgent.id, {
      version: managedAgent.version,
      name: 'sdk-authored-agent-v2',
      system: 'You survived a process restart.',
      betas: BETAS,
    });
    assert.equal(managedUpdated.version, 2);
    r = await req(base, 'GET', `/v1/workspaces/foreign-workspace/agents/${managedAgent.id}`);
    assert.equal(r.status, 404, 'a foreign Workspace cannot retrieve the Agent');
    console.log('ok: Managed Agent SDK writes use ConfigPlane CAS and Workspace routing');

    r = await req(base, 'PUT', '/v1/config/agents/unpublished-model-agent', {
      name: 'unpublished-model-agent',
      model: { id: 'model-with-no-offering' },
      system: 'This publication must fail closed.',
    });
    assert.equal(r.status, 200, `unpublished model draft: ${JSON.stringify(r.json)}`);
    r = await req(base, 'POST', '/v1/config/agents/unpublished-model-agent/publish', undefined);
    assert.equal(r.status, 409, `unpublished model must not install: ${JSON.stringify(r.json)}`);

    // Mutating the catalog after publication cannot redirect the installed
    // snapshot: execution must still call the endpoint pinned above.
    r = await req(base, 'PUT', '/v1/config/endpoints/ep1', {
      id: 'ep1', provider_id: 'anthropic', dialect: 'anthropic_messages',
      base_url: 'http://127.0.0.1:1/v1/', timeout_secs: 300, display_name: 'mutated', version: 2,
    });
    assert.equal(r.status, 200, `post-publication endpoint mutation: ${JSON.stringify(r.json)}`);

    // ---- run a session on the DB-configured model ----------------------------
    const session = await client.beta.sessions.create({
      agent: AGENT, environment_id: 'env_local', betas: BETAS,
    });
    assert.ok(session.id.startsWith('sesn_'), `session id: ${session.id}`);
    await client.beta.sessions.events.send(session.id, {
      events: [{ type: 'user.message', content: [{ type: 'text', text: 'resolve me' }] }],
      betas: BETAS,
    });
    const events = [];
    for await (const ev of client.beta.sessions.events.list(session.id, { betas: BETAS })) events.push(ev);
    const msg = events.find((e) => e.type === 'agent.message');
    assert.ok(msg, `expected an agent.message in ${events.map((e) => e.type)}`);
    const text = (msg.content ?? []).map((c) => c.text ?? '').join('');
    assert.ok(
      text.includes('FAKE:resolve me'),
      `the session ran the DB-configured model over the wire: ${JSON.stringify(text)}`,
    );
    assert.ok(upstream.requests.length >= 1, 'the fake upstream received the configured-model call');
    console.log('ok: session used the snapshot-pinned endpoint despite a later catalog mutation');

    // A fresh composition warm-installs the durable publication. It must retain
    // the original snapshot pin rather than resolving the mutated catalog again.
    await h.stop();
    h = startAwaken(bin, PORT, serverEnv);
    await waitForPort(PORT);
    base = h.baseUrl;
    await ready(base);
    client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: base });
    const durableManaged = await client.beta.agents.retrieve(managedAgent.id, { betas: BETAS });
    assert.equal(durableManaged.name, 'sdk-authored-agent-v2');
    assert.equal(durableManaged.version, 2);
    const durableVersions = [];
    for await (const version of client.beta.agents.versions.list(managedAgent.id, { betas: BETAS })) {
      durableVersions.push(version);
    }
    assert.deepEqual(durableVersions.map((version) => version.version), [1, 2]);
    const managedSession = await client.beta.sessions.create({
      agent: managedAgent.id, environment_id: 'env_local', betas: BETAS,
    });
    assert.equal(managedSession.agent.model.id, MODEL);
    assert.equal(managedSession.agent.system, 'You survived a process restart.');
    console.log('ok: SDK Agent revisions and execution projection survived process restart');

    const archivedManaged = await client.beta.agents.archive(managedAgent.id, { betas: BETAS });
    assert.ok(archivedManaged.archived_at);
    await assert.rejects(
      () => client.beta.sessions.create({
        agent: managedAgent.id, environment_id: 'env_local', betas: BETAS,
      }),
      (error) => error.status === 400 && String(error.message).includes('agent_archived'),
    );
    await h.stop();
    h = startAwaken(bin, PORT, serverEnv);
    await waitForPort(PORT);
    base = h.baseUrl;
    await ready(base);
    client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: base });
    const archivedAfterRestart = await client.beta.agents.retrieve(managedAgent.id, { betas: BETAS });
    assert.ok(archivedAfterRestart.archived_at, 'archive lifecycle survives restart');
    await assert.rejects(
      () => client.beta.sessions.create({
        agent: managedAgent.id, environment_id: 'env_local', betas: BETAS,
      }),
      (error) => error.status === 400 && String(error.message).includes('agent_archived'),
    );
    console.log('ok: archived Agent history and execution denial survive process restart');

    const warm = await client.beta.sessions.create({
      agent: AGENT, environment_id: 'env_local', betas: BETAS,
    });
    await client.beta.sessions.events.send(warm.id, {
      events: [{ type: 'user.message', content: [{ type: 'text', text: 'warm install' }] }],
      betas: BETAS,
    });
    const warmEvents = [];
    for await (const ev of client.beta.sessions.events.list(warm.id, { betas: BETAS })) warmEvents.push(ev);
    assert.ok(warmEvents.some((event) => event.type === 'agent.message'), 'warm-installed snapshot executes');
    console.log('ok: durable publication warm-installed after restart without re-resolution');

    // Restore the live catalog endpoint after proving the ordinary Agent retained
    // its older pin. The catalog-write reconciler republishes the reserved Admin
    // Assistant against this current endpoint; then drive its six real management
    // adapters through the ordinary Sessions API in the production composition.
    r = await req(base, 'PUT', '/v1/config/endpoints/ep1', {
      id: 'ep1', provider_id: 'anthropic', dialect: 'anthropic_messages',
      base_url: `${upstream.url}/v1/`, timeout_secs: 300, display_name: 'restored', version: 3,
    });
    assert.equal(r.status, 200, `restore endpoint: ${JSON.stringify(r.json)}`);
    const adminDeadline = Date.now() + 10_000;
    let projectedAdmin;
    do {
      projectedAdmin = await req(base, 'GET', '/v1/agents/__admin_assistant');
      if (projectedAdmin.status === 200) break;
      await sleep(100);
    } while (Date.now() < adminDeadline);
    assert.equal(projectedAdmin.status, 200, `admin assistant projection: ${JSON.stringify(projectedAdmin.json)}`);
    const adminSession = await client.beta.sessions.create({
      agent: '__admin_assistant', environment_id: 'env_local', betas: BETAS,
    });
    await client.beta.sessions.events.send(adminSession.id, {
      events: [{ type: 'user.message', content: [{ type: 'text', text: 'author an agent and environment' }] }],
      betas: BETAS,
    });
    const adminEvents = [];
    for await (const event of client.beta.sessions.events.list(adminSession.id, { betas: BETAS })) {
      adminEvents.push(event);
    }
    const adminTranscript = JSON.stringify(adminEvents);
    for (const toolId of [
      'admin_get_platform_capabilities',
      'admin_draft_agent',
      'admin_patch_agent',
      'admin_validate_agent',
      'admin_draft_environment',
      'admin_explain_console',
    ]) assert.ok(adminTranscript.includes(toolId), `production Admin Assistant invoked ${toolId}`);
    assert.ok(adminTranscript.includes('ADMIN-RUN-DONE'), 'production Admin Assistant completed its tool loop');
    const draftedResources = await req(base, 'GET', '/v1/config/agents/drafted-agent/resources');
    assert.equal(draftedResources.status, 200, JSON.stringify(draftedResources.json));
    assert.equal(draftedResources.json.inputs.length, 1);
    assert.deepEqual(draftedResources.json.inputs[0].target, {
      kind: 'file', id: 'replacement-file',
    });
    assert.equal(draftedResources.json.inputs[0].access, 'read_only', 'File access is monotonically narrowed');
    const environments = await req(base, 'GET', '/v1/environments');
    assert.equal(environments.status, 200, JSON.stringify(environments.json));
    assert.ok(
      environments.json.data.some((environment) => environment.name === 'admin-authored-environment'),
      'Admin Assistant persisted the Environment through the production registry',
    );
    console.log('ok: production Admin Assistant drove all six management adapters');

    const callsBeforeRevocation = upstream.requests.length;
    r = await req(base, 'POST', `/v1/config/credentials/${credentialId}/archive`, undefined);
    assert.equal(r.status, 200, `archive credential: ${JSON.stringify(r.json)}`);
    const rejected = await client.beta.sessions.create({
      agent: AGENT, environment_id: 'env_local', betas: BETAS,
    });
    await client.beta.sessions.events.send(rejected.id, {
      events: [{ type: 'user.message', content: [{ type: 'text', text: 'must fail closed' }] }],
      betas: BETAS,
    });
    const rejectedEvents = [];
    for await (const ev of client.beta.sessions.events.list(rejected.id, { betas: BETAS })) rejectedEvents.push(ev);
    assert.ok(rejectedEvents.some((event) => event.type === 'session.error'), 'revoked pin produces session.error');
    assert.ok(!rejectedEvents.some((event) => event.type === 'agent.message'), 'revoked pin produces no model reply');
    assert.equal(upstream.requests.length, callsBeforeRevocation, 'revoked pin never reaches any endpoint');
    console.log('ok: archived/version-changed credential rejected the already-published pin fail-closed');

    // Defense in depth without IAM: workspace-path addressing supplies the
    // trusted platform scope directly, and each resource aggregate enforces its
    // own persisted owner. This proves the resource service does not depend on a
    // PEP-side process-local owner index.
    const WS_A = 'workspace-resource-a';
    const WS_B = 'workspace-resource-b';
    const scoped = (workspace, tail) => `/v1/workspaces/${workspace}/config/${tail}`;
    const ownedCredential = await req(base, 'POST', scoped(WS_A, 'credentials'), {
      workspace_id: 'forged-body-owner', kind: 'vault', provider_id: 'anthropic',
      env_key: null, secret: 'sk-resource-owner-e2e', // awaken-allow: secret
    });
    assert.equal(ownedCredential.status, 201, JSON.stringify(ownedCredential.json));
    assert.equal(ownedCredential.json.workspace_id, WS_A, 'path scope overrides body workspace');
    const ownedId = ownedCredential.json.id;
    const pool = {
      id: 'owned-pool', workspace_id: 'forged-body-owner',
      members: [{ credential_source_id: ownedId, ordinal: 0, enabled: true, selection_weight: 0 }],
    };
    assert.equal((await req(base, 'PUT', scoped(WS_A, 'credential-pools/owned-pool'), pool)).status, 200);
    const profile = {
      workspace_id: 'forged-body-owner', model_id: MODEL,
      credential_binding: { type: 'exact', credential_source_id: ownedId }, disabled_endpoint_ids: [],
    };
    assert.equal((await req(base, 'PUT', scoped(WS_A, 'inference-profiles/owned-profile'), profile)).status, 200);
    const mcp = {
      id: 'owned-mcp', workspace_id: 'forged-body-owner', display_name: 'owned',
      url: 'https://mcp.example.invalid/',
      credential_binding: { type: 'exact', credential_source_id: ownedId }, version: 1,
    };
    assert.equal((await req(base, 'PUT', scoped(WS_A, 'mcp-servers/owned-mcp'), mcp)).status, 200);
    const agentMcp = {
      workspace_id: 'forged-body-owner', agent_id: 'owned-agent',
      mcp_server_ids: ['owned-mcp'], version: 1,
    };
    assert.equal((await req(base, 'PUT', scoped(WS_A, 'agents/owned-agent/mcp'), agentMcp)).status, 200);

    // Exercise the owning side of every scoped operational route as well as
    // cross-aggregate binding checks. A foreign credential/pool/server must not
    // become usable merely because the new aggregate id itself is unclaimed.
    assert.equal((await req(base, 'GET', scoped(WS_A, `credentials/${ownedId}/availability`))).status, 200);
    assert.equal((await req(base, 'POST', scoped(WS_A, `credentials/${ownedId}/cooldown`), {
      kind: 'transient', retry_after_secs: null,
    })).status, 200);
    assert.equal((await req(base, 'POST', scoped(WS_A, `credentials/${ownedId}/cooldown`), {
      kind: 'quota', retry_after_secs: 60,
    })).status, 200);
    assert.equal((await req(base, 'POST', scoped(WS_A, `credentials/${ownedId}/cooldown`), {
      kind: 'available', retry_after_secs: null,
    })).status, 200);
    assert.equal((await req(base, 'POST', scoped(WS_A, `credentials/${ownedId}/cooldown`), {
      kind: 'exhausted', retry_after_secs: null,
    })).status, 200);
    assert.equal((await req(base, 'POST', scoped(WS_A, `credentials/${ownedId}/cooldown`), {
      kind: 'clear', retry_after_secs: null,
    })).status, 200);
    assert.equal((await req(base, 'POST', scoped(WS_A, 'inference-profiles/owned-profile/resolve'), {
      workspace_id: 'forged-body-owner',
    })).status, 200);
    assert.equal((await req(base, 'POST', scoped(WS_A, 'inference-profiles/owned-profile/resolve-candidates'), {
      workspace_id: 'forged-body-owner',
    })).status, 200);
    assert.equal((await req(base, 'GET', scoped(WS_A, `credentials/${ownedId}`))).status, 200);
    const credentials = await req(base, 'GET', scoped(WS_A, 'credentials?workspace_id=forged-body-owner'));
    assert.equal(credentials.status, 200);
    assert.ok(credentials.json.some((entry) => entry.id === ownedId));
    assert.equal((await req(base, 'POST', scoped(WS_A, `credentials/${ownedId}/validate`), {
      workspace_id: 'forged-body-owner', model_id: MODEL,
    })).status, 200);
    assert.equal((await req(base, 'GET', scoped(WS_A, 'mcp-servers/owned-mcp'))).status, 200);
    const owningMcp = await req(base, 'GET', scoped(WS_A, 'mcp-servers'));
    assert.equal(owningMcp.status, 200);
    assert.ok(owningMcp.json.some((entry) => entry.id === 'owned-mcp'));
    assert.equal((await req(base, 'GET', scoped(WS_A, 'agents/owned-agent/mcp'))).status, 200);
    const resolvedMcp = await req(base, 'POST', scoped(WS_A, 'agents/owned-agent/mcp/resolve'), {
      workspace_id: 'forged-body-owner',
    });
    assert.equal(resolvedMcp.status, 200, JSON.stringify(resolvedMcp.json));
    assert.deepEqual(resolvedMcp.json, [{
      name: 'owned', url: 'https://mcp.example.invalid/', credential_present: true,
    }]);

    const foreignExactMcp = {
      ...mcp, id: 'foreign-exact-mcp', workspace_id: WS_B,
    };
    assert.equal((await req(base, 'PUT', scoped(WS_B, 'mcp-servers/foreign-exact-mcp'), foreignExactMcp)).status, 404);
    const foreignPoolMcp = {
      ...mcp, id: 'foreign-pool-mcp', workspace_id: WS_B,
      credential_binding: { type: 'one_of_credential_pool', credential_pool_id: 'owned-pool' },
    };
    assert.equal((await req(base, 'PUT', scoped(WS_B, 'mcp-servers/foreign-pool-mcp'), foreignPoolMcp)).status, 404);
    assert.equal((await req(base, 'PUT', scoped(WS_B, 'agents/foreign-agent/mcp'), {
      workspace_id: WS_B, agent_id: 'foreign-agent', mcp_server_ids: ['owned-mcp'], version: 1,
    })).status, 404);

    for (const uri of [
      scoped(WS_B, `credentials/${ownedId}`),
      scoped(WS_B, 'credential-pools/owned-pool'),
      scoped(WS_B, 'credential-pools/owned-pool/eligible'),
      scoped(WS_B, 'inference-profiles/owned-profile'),
      scoped(WS_B, 'mcp-servers/owned-mcp'),
      scoped(WS_B, 'agents/owned-agent/mcp'),
    ]) assert.equal((await req(base, 'GET', uri)).status, 404, `${uri} hides foreign ownership`);
    assert.equal((await req(base, 'PUT', scoped(WS_B, 'credential-pools/owned-pool'), pool)).status, 404);
    assert.equal((await req(base, 'PUT', scoped(WS_B, 'inference-profiles/owned-profile'), profile)).status, 404);
    assert.equal((await req(base, 'PUT', scoped(WS_B, 'mcp-servers/owned-mcp'), mcp)).status, 404);
    assert.equal((await req(base, 'PUT', scoped(WS_B, 'agents/owned-agent/mcp'), agentMcp)).status, 404);
    assert.equal((await req(base, 'POST', scoped(WS_B, 'inference-profiles/owned-profile/resolve-candidates'), {
      workspace_id: WS_A,
    })).status, 404);
    assert.equal((await req(base, 'POST', scoped(WS_B, 'agents/owned-agent/mcp/resolve'), {
      workspace_id: WS_A,
    })).status, 404);
    const listedMcp = await req(base, 'GET', scoped(WS_B, 'mcp-servers'));
    assert.equal(listedMcp.status, 200);
    assert.ok(!listedMcp.json.some((entry) => entry.id === 'owned-mcp'));

    const upload = new FormData();
    upload.set('file', new Blob(['workspace-owned-file']), 'owned.txt');
    const uploaded = await fetch(`${base}/v1/workspaces/${WS_A}/files`, { method: 'POST', body: upload });
    assert.equal(uploaded.status, 200);
    const fileId = (await uploaded.json()).id;
    assert.equal((await fetch(`${base}/v1/workspaces/${WS_A}/files/${fileId}`)).status, 200);
    assert.equal((await fetch(`${base}/v1/workspaces/${WS_B}/files/${fileId}`)).status, 404);
    console.log('ok: config resources and files enforce intrinsic workspace ownership without IAM');

    // Exercise the production embedded Resource Catalog adapter through the same
    // Managed Session edge used by cloud mode. Only the persistence adapter differs.
    const repository = seedRepository(mgmtDir);
    const repositoryResource = await client.beta.sessions.resources.add(warm.id, {
      type: 'github_repository',
      url: repository,
      mount_path: '/workspace/repository',
      betas: BETAS,
    });
    const updatedRepository = await client.beta.sessions.resources.update(repositoryResource.id, {
      session_id: warm.id,
      mount_path: '/workspace/repository-updated',
      authorization_token: 'sqlite-repository-rotated-token', // awaken-allow: secret
      betas: BETAS,
    });
    assert.equal(updatedRepository.mount_path, '/workspace/repository-updated');
    const retiredRepository = await client.beta.sessions.resources.delete(repositoryResource.id, {
      session_id: warm.id,
      betas: BETAS,
    });
    assert.equal(retiredRepository.type, 'session_resource_deleted');
    console.log('ok: embedded repository configuration publishes and retires through SQLite');
  } finally {
    await h.stop();
    upstream.close();
    fs.rmSync(mgmtDir, { recursive: true, force: true });
  }
  console.log('\nawaken_cli_e2e: PASS');
}

main().catch((err) => {
  console.error(err);
  process.exit(1);
});
