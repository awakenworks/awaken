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
import os from 'node:os';
import readline from 'node:readline';
import { spawn, execFileSync } from 'node:child_process';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import Anthropic from '@anthropic-ai/sdk';
import { startFakeAnthropic } from './fixtures/fake_anthropic_fixture.mjs';
import { automatedAllInOneArgs } from './awaken_cli_args.mjs';
import { AWAKEN_BIN_ENV, cargoExecutable } from './cargo_binary.mjs';
import {
  managedFileUploadForm,
  waitForSessionEventReceipt,
  waitForValue,
} from './harness.mjs';

const REPO_ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const PORT = Number(process.env.E2E_PORT ?? 38411);
const BETAS = ['managed-agents-2026-04-01'];
const FAKE_KEY = 'sk-awaken-cli-fake-key'; // awaken-allow: secret
// The composition root receives the platform-owned coordinate explicitly; no test
// or resource adapter relies on a compiled Workspace id.
let WORKSPACE;
const AGENT = 'db-model-agent';
const MODEL = 'fake-haiku';
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

function awakenBin() {
  return cargoExecutable({
    cwd: REPO_ROOT,
    packageName: 'awaken-cli',
    targetName: 'awaken',
    prebuiltEnvironmentName: AWAKEN_BIN_ENV,
  });
}

function startAwaken(bin, port, configPath, extraEnv = {}) {
  // This is a process-lifecycle test, not a browser-launch test. Keeping the
  // product opener enabled can leave a desktop/browser descendant holding the
  // harness PTY after Awaken itself has shut down and make a passing run hang.
  const server = spawn(bin, automatedAllInOneArgs('--config', configPath, '--port', String(port)), {
    env: { ...process.env, ...extraEnv },
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
    headers: {
      'anthropic-beta': BETAS[0],
      ...(body === undefined ? {} : { 'content-type': 'application/json' }),
    },
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
  // Startup-probe decision table: C1 HTTP exchange completes -> ready, even
  // when auth/beta policy rejects this anonymous request; C2 transport fails ->
  // retry until the deadline. Product authorization is tested after bootstrap.
  const deadline = Date.now() + timeoutMs;
  for (;;) {
    try {
      await fetch(`${base}/v1/capabilities`);
      return;
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
  execFileSync('git', ['init', '-q'], { cwd: work });
  execFileSync('git', ['symbolic-ref', 'HEAD', 'refs/heads/main'], { cwd: work });
  execFileSync('git', ['config', 'user.email', 'resource-e2e@example.invalid'], { cwd: work });
  execFileSync('git', ['config', 'user.name', 'resource-e2e'], { cwd: work });
  fs.writeFileSync(path.join(work, 'README.md'), 'sqlite resource catalog');
  execFileSync('git', ['add', 'README.md'], { cwd: work });
  execFileSync('git', ['commit', '-q', '-m', 'seed'], { cwd: work });
  execFileSync('git', ['clone', '-q', '--bare', work, remote]);
  return remote;
}

async function main() {
  const upstream = await startFakeAnthropic(FAKE_KEY, { models: [MODEL] });
  const bin = awakenBin();
  const mgmtDir = fs.mkdtempSync(path.join(os.tmpdir(), 'awaken-cli-e2e-'));
  const configPath = path.join(mgmtDir, 'config.toml');
  fs.writeFileSync(configPath, [
    `data_dir = ${JSON.stringify(mgmtDir)}`,
    'control_seal_key = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff"',
    // This suite owns the resource-service isolation matrix. Authentication and
    // IAM authorization are covered separately by management_authz_e2e.mjs.
    'identity_mode = "no-login"',
    'sandbox_tier = "local"',
    // Keep this production-composition fixture independent of ACP CLIs installed
    // on the developer host. The Admin Assistant under test is republished onto
    // the authored fake provider below; ambient Codex discovery must not replace
    // that deterministic model with an unverified Worker capability.
    'acp_clis = ["gemini"]',
  ].join('\n'));
  const serverEnv = {};
  let h = startAwaken(bin, PORT, configPath, serverEnv);
  try {
    await ready(h.baseUrl);
    WORKSPACE = fs.readFileSync(path.join(mgmtDir, 'platform-workspace-id'), 'utf8').trim();
    console.log('ok: aggregated `awaken` command booted in the default Serve role (management plane)');

    // ---- connect the provider through the canonical atomic command -----------
    let base = h.baseUrl;
    let r = await req(base, 'POST', '/v1/config/provider-connections', {
      idempotency_key: 'awaken-cli-e2e-initial-provider',
      workspace_id: WORKSPACE,
      provider_id: 'anthropic',
      display_name: 'Anthropic',
      dialect: 'anthropic_messages',
      base_url: `${upstream.url}/v1/`,
      timeout_secs: 300,
      secret: FAKE_KEY,
    });
    assert.equal(r.status, 201, `provider connection: ${JSON.stringify(r.json)}`);
    const providerCredentialId = r.json.credential.id;
    const providerCredentialVersion = r.json.credential.version;
    r = await req(base, 'PUT', `/v1/config/model-attributes/${MODEL}`, {
      context_window: 4096,
      max_output_tokens: 1024,
    });
    assert.equal(r.status, 200, `model attributes: ${JSON.stringify(r.json)}`);
    const credentialId = providerCredentialId;
    console.log('ok: authored provider/endpoint/offering + credential in the console DB');

    // The managed projection accepts both SDK-shaped tool objects and string ids,
    // filters non-string metadata, and rejects malformed typed policy extensions.
    const projectionAgent = 'managed-projection-edge';
    r = await req(base, 'PUT', `/v1/config/agents/${projectionAgent}`, {
      name: projectionAgent,
      model: { id: MODEL },
      tools: [{ id: 'glob' }, { name: 'bash' }, 42],
      metadata: { owner: 'platform', ignored_number: 7 },
    });
    assert.equal(r.status, 200, `managed projection draft: ${JSON.stringify(r.json)}`);
    r = await req(base, 'GET', `/v1/config/agents/${projectionAgent}`, undefined);
    assert.equal(r.status, 200, `managed projection read: ${JSON.stringify(r.json)}`);
    assert.deepEqual(r.json.tools, ['glob', 'bash']);
    assert.deepEqual(r.json.metadata, { owner: 'platform' });

    for (const malformed of [{ context_policy: 123 }, { tool_overrides: 123 }]) {
      r = await req(base, 'PUT', `/v1/config/agents/${projectionAgent}`, {
        name: projectionAgent,
        model: { id: MODEL },
        ...malformed,
      });
      assert.equal(r.status, 400, `malformed managed extension fails closed: ${JSON.stringify(r.json)}`);
    }
    console.log('ok: managed agent object/tool/metadata projection is lossless and typed extensions fail closed');

    // ---- publish an agent bound to that model --------------------------------
    // The console agent object is the managed `/v1/agents` shape: the model is
    // `model: { id }`, which the config plane maps to a Pinned selection — so a
    // session for this agent runs exactly the DB-configured `MODEL`.
    r = await req(base, 'PUT', `/v1/config/agents/${AGENT}`, {
      name: AGENT,
      model: { id: MODEL },
      system: 'You are a test agent.',
      max_steps: 2,
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

    // Agent-input grammar decision table: C1=a PUT uses the removed root
    // `resources/version` grammar; C2=a later PUT uses canonical `inputs/revision`.
    // E1=C1 is rejected with exact 422 before any repository write; E2=a GET of
    // that Agent remains 404; E3=C2 is accepted and remains the positive publish
    // proof below. K1=AgentInputConfig is the sole strict wire/persistence shape;
    // K2=no compatibility decoder may reinterpret a legacy body.
    //
    // | Rule | Legacy grammar | Canonical grammar | Effect |
    // |---|---|---|---|
    // | AI1 | yes | no | E1,E2 |
    // | AI2 | no | yes | E3 |
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
    assert.equal(r.status, 422, `AI1 legacy Agent-input grammar: ${JSON.stringify(r.json)}`);
    r = await req(base, 'GET', `/v1/config/agents/${legacyAgent}/resources`);
    assert.equal(r.status, 404, `AI1 legacy body must not persist: ${JSON.stringify(r.json)}`);
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

    // A replacement connection is tested before commit. A failed probe cannot
    // redirect either the live catalog or the already-installed snapshot.
    r = await req(base, 'POST', '/v1/config/provider-connections', {
      idempotency_key: 'awaken-cli-e2e-rejected-replacement',
      workspace_id: WORKSPACE,
      provider_id: 'anthropic',
      display_name: 'Anthropic',
      dialect: 'anthropic_messages',
      base_url: 'http://127.0.0.1:1/v1/',
      timeout_secs: 300,
      credential_source_id: providerCredentialId,
    });
    assert.equal(r.status, 502, `connection test rejects an unreachable replacement: ${JSON.stringify(r.json)}`);

    // ---- run a session on the DB-configured model ----------------------------
    // Cause-effect graph for direct-attempt credential authority:
    // C1 published candidate has credential + C2 selected holder is admitted
    // C2 + C3 current local run owns its fence -> E1 compile binding, materialize,
    // record the receipt, and invoke the exact pinned endpoint.
    // !C2 or !C3 -> E2 fail closed before provider I/O; no unbound fallback.
    //
    // | Rule | Credential | Holder admitted | Ownership | Result       |
    // | L1   | yes        | yes             | current   | exact invoke |
    // | L2   | yes        | no              | -         | reject       |
    // | L3   | yes        | yes             | stale     | reject       |
    const session = await client.beta.sessions.create({
      agent: AGENT, environment_id: 'env_local', betas: BETAS,
    });
    assert.ok(session.id.startsWith('sesn_'), `session id: ${session.id}`);
    // Managed execution receipt rules: C1=exact User receipt; C2=scenario
    // reply/error terminal. E1=C2 is eligible only after C1. K: Control and
    // Resource projections remain separate read authorities. Decision L1
    // C1&&!C2=>retry; L2 C1+C2=>assert the scenario-owned effect.
    const sessionReceipt = await client.beta.sessions.events.send(session.id, {
      events: [{ type: 'user.message', content: [{ type: 'text', text: 'resolve me' }] }],
      betas: BETAS,
    });
    const sessionReceiptId = sessionReceipt.data[0]?.id;
    assert.equal(typeof sessionReceiptId, 'string', 'L1 exact configured-model User Event receipt');
    const { delta: events } = await waitForSessionEventReceipt(
      client,
      session.id,
      sessionReceiptId,
      BETAS,
      ({ delta }) => delta.some((event) => event.type === 'agent.message')
        && delta.some((event) => event.type === 'session.status_idle'),
      'L1 configured-model Run to commit',
      { timeoutMs: 30_000 },
    );
    const msg = events.find((e) => e.type === 'agent.message');
    assert.ok(msg, `expected an agent.message in ${JSON.stringify(events)}`);
    const text = (msg.content ?? []).map((c) => c.text ?? '').join('');
    assert.ok(
      text.includes('FAKE:resolve me'),
      `the session ran the DB-configured model over the wire: ${JSON.stringify(text)}`,
    );
    assert.ok(upstream.requests.length >= 1, 'the fake upstream received the configured-model call');
    console.log('ok: session used the snapshot-pinned endpoint despite a later catalog mutation');

    // Restart/shutdown cause-effect graph: C1 the AllInOne Worker owns an active
    // registry generation; C2 SIGINT requests process shutdown; C3 Control stays
    // reachable through Worker drain/quiesce/deregister. C1+C2+C3 -> E1 the
    // immediate replacement registers a new generation and becomes ready. If C3
    // is false, the stale generation fences replacement and readiness times out.
    //
    // | Rule | C1 active Worker | C2 SIGINT | C3 Control through deregister | E1 restart ready |
    // | R1   | T                | T         | T                            | T                |
    //
    // The fresh composition also warm-installs the durable publication. It must
    // retain the original snapshot pin rather than resolving the mutated catalog.
    await h.stop();
    h = startAwaken(bin, PORT, configPath, serverEnv);
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
    // Archive fence outcome: both the live and warm-restored projection expose
    // the same opaque `agent_unavailable` execution denial; lifecycle detail is
    // visible through Agent retrieval, not leaked through Session creation.
    await assert.rejects(
      () => client.beta.sessions.create({
        agent: managedAgent.id, environment_id: 'env_local', betas: BETAS,
      }),
      (error) => error.status === 400 && String(error.message).includes('cannot start a new session'),
    );
    await h.stop();
    h = startAwaken(bin, PORT, configPath, serverEnv);
    base = h.baseUrl;
    await ready(base);
    client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: base });
    const archivedAfterRestart = await client.beta.agents.retrieve(managedAgent.id, { betas: BETAS });
    assert.ok(archivedAfterRestart.archived_at, 'archive lifecycle survives restart');
    await assert.rejects(
      () => client.beta.sessions.create({
        agent: managedAgent.id, environment_id: 'env_local', betas: BETAS,
      }),
      (error) => error.status === 400 && String(error.message).includes('cannot start a new session'),
    );
    console.log('ok: archived Agent history and execution denial survive process restart');

    const warm = await client.beta.sessions.create({
      agent: AGENT, environment_id: 'env_local', betas: BETAS,
    });
    const warmReceipt = await client.beta.sessions.events.send(warm.id, {
      events: [{ type: 'user.message', content: [{ type: 'text', text: 'warm install' }] }],
      betas: BETAS,
    });
    const warmReceiptId = warmReceipt.data[0]?.id;
    assert.equal(typeof warmReceiptId, 'string', 'L2 exact warm-install User Event receipt');
    const { delta: warmEvents } = await waitForSessionEventReceipt(
      client,
      warm.id,
      warmReceiptId,
      BETAS,
      ({ delta }) => delta.some((event) => event.type === 'agent.message')
        && delta.some((event) => event.type === 'session.status_idle'),
      'L2 warm-installed publication Run to commit',
      { timeoutMs: 30_000 },
    );
    assert.ok(warmEvents.some((event) => event.type === 'agent.message'), 'warm-installed snapshot executes');
    console.log('ok: durable publication warm-installed after restart without re-resolution');

    // The rejected replacement left the connected endpoint unchanged. The
    // connection-write reconciler republishes the reserved Admin Assistant
    // against that endpoint. It is an ordinary Agent with no hidden policy
    // overlay, so this composition must drive its complete management tool chain.
    const projectedAdmin = await waitForValue(
      () => req(base, 'GET', '/v1/agents/__admin_assistant'),
      (projection) => projection.status === 200,
      'Admin Assistant projection to become readable',
      { timeoutMs: 10_000 },
    );
    assert.equal(projectedAdmin.status, 200, `admin assistant projection: ${JSON.stringify(projectedAdmin.json)}`);
    const adminSession = await client.beta.sessions.create({
      agent: '__admin_assistant', environment_id: 'env_local', betas: BETAS,
    });
    const adminReceipt = await client.beta.sessions.events.send(adminSession.id, {
      events: [{ type: 'user.message', content: [{ type: 'text', text: 'author an agent and environment' }] }],
      betas: BETAS,
    });
    const adminReceiptId = adminReceipt.data[0]?.id;
    assert.equal(typeof adminReceiptId, 'string', 'L3 exact Admin Assistant User Event receipt');
    const { delta: adminEvents } = await waitForSessionEventReceipt(
      client,
      adminSession.id,
      adminReceiptId,
      BETAS,
      ({ delta }) => JSON.stringify(delta).includes('ADMIN-RUN-DONE')
        && [...delta].reverse().find((event) => event.type === 'session.status_idle')?.stop_reason?.type === 'end_turn',
      'L3 production Admin Assistant Run to commit',
      { timeoutMs: 30_000 },
    );
    const adminTranscript = JSON.stringify(adminEvents);
    for (const tool of [
      'admin_get_platform_capabilities',
      'admin_draft_agent',
      'admin_patch_agent',
      'admin_validate_agent',
      'admin_draft_environment',
      'admin_explain_console',
    ]) {
      assert.ok(adminTranscript.includes(tool), `Admin Assistant invoked ${tool}: ${adminTranscript}`);
    }
    assert.ok(
      adminEvents.some((event) => event.type === 'session.status_idle'
        && event.stop_reason?.type === 'end_turn'),
      `production Admin Assistant completed naturally: ${adminTranscript}`,
    );
    assert.ok(adminTranscript.includes('ADMIN-RUN-DONE'), adminTranscript);
    console.log('ok: production Admin Assistant completes its ordinary audited management tool chain');

    // Exercise the production embedded Resource Catalog adapter through the same
    // Managed Session edge used by cloud mode. Only the persistence adapter differs.
    // Resource-realization cause graph: C1 the repository definition resolves;
    // C2 a Worker owns the exact Session realization lease; C3 the pinned Agent
    // credential is current; C4 a later credential revocation occurs only after
    // this activation has settled. Effects: C1+C2+C3 activates one projected
    // repository; !C2 remains pending; !C3 fails closed without manufacturing an
    // active Resource; C4 cannot retroactively invalidate the completed proof.
    //
    // | Rule | Repository | Worker lease | Current credential | Effect |
    // |---|---|---|---|---|
    // | RC1 | valid | current | yes | one active Resource, then retire |
    // | RC2 | valid | absent/stale | yes | pending, never projected active |
    // | RC3 | valid | current | no | realization fails closed |
    const repository = seedRepository(mgmtDir);
    const repositorySession = await client.beta.sessions.create({
      agent: AGENT,
      environment_id: 'env_local',
      resources: [{
        type: 'github_repository',
        url: repository,
        mount_path: '/workspace/repository',
      }],
      betas: BETAS,
    });
    const repositoryReceipt = await client.beta.sessions.events.send(repositorySession.id, {
      events: [{
        type: 'user.message',
        content: [{ type: 'text', text: 'activate the repository projection' }],
      }],
      betas: BETAS,
    });
    const repositoryReceiptId = repositoryReceipt.data[0]?.id;
    assert.equal(typeof repositoryReceiptId, 'string', 'RC1 exact repository activation receipt');
    await waitForSessionEventReceipt(
      client,
      repositorySession.id,
      repositoryReceiptId,
      BETAS,
      ({ delta }) => delta.some((event) => event.type === 'session.status_idle'),
      'RC1 repository activation Run to commit',
      { timeoutMs: 30_000 },
    );
    const repositoryResource = await waitForValue(
      async () => {
        const projection = await client.beta.sessions.retrieve(repositorySession.id, { betas: BETAS });
        return projection.resources.find((resource) => resource.type === 'github_repository');
      },
      Boolean,
      'RC1 repository resource to project on the Session',
      { timeoutMs: 30_000 },
    );
    assert.ok(repositoryResource?.id);
    const retiredRepository = await client.beta.sessions.resources.delete(repositoryResource.id, {
      session_id: repositorySession.id,
      betas: BETAS,
    });
    assert.equal(retiredRepository.type, 'session_resource_deleted');
    console.log('ok: create-time repository configuration publishes and retires through SQLite');

    const callsBeforeRevocation = upstream.requests.length;
    r = await req(base, 'POST', `/v1/config/credentials/${credentialId}/archive`, {
      expected_version: providerCredentialVersion,
    });
    assert.equal(r.status, 200, `archive credential: ${JSON.stringify(r.json)}`);
    const rejected = await client.beta.sessions.create({
      agent: AGENT, environment_id: 'env_local', betas: BETAS,
    });
    const rejectedReceipt = await client.beta.sessions.events.send(rejected.id, {
      events: [{ type: 'user.message', content: [{ type: 'text', text: 'must fail closed' }] }],
      betas: BETAS,
    });
    const rejectedReceiptId = rejectedReceipt.data[0]?.id;
    assert.equal(typeof rejectedReceiptId, 'string', 'L4 exact revoked-pin User Event receipt');
    const { delta: rejectedEvents } = await waitForSessionEventReceipt(
      client,
      rejected.id,
      rejectedReceiptId,
      BETAS,
      ({ delta }) => delta.some((event) => event.type === 'session.error'),
      'L4 revoked-pin Run to commit its fail-closed error',
      { timeoutMs: 30_000 },
    );
    assert.ok(rejectedEvents.some((event) => event.type === 'session.error'), 'revoked pin produces session.error');
    assert.ok(!rejectedEvents.some((event) => event.type === 'agent.message'), 'revoked pin produces no model reply');
    assert.equal(upstream.requests.length, callsBeforeRevocation, 'revoked pin never reaches any endpoint');
    console.log('ok: archived/version-changed credential rejected the already-published pin fail-closed');

    // Cause graph (production composition Workspace ownership):
    //   C1 path Workspace differs from body -> E1 trusted path stamps ownership
    //   C2 B reads A-owned aggregate       -> E2 hide it with 404
    //   C3 same logical id authored in B   -> E3 independent scoped aggregate
    //   C4 B draft references A credential -> E4 publication fails closed
    //   C5 B authors same profile id       -> E5 independent B profile + resolution
    // Decision table: T1=C1/E1, T2=C2/E2, T3=C3/E3, T4=C2+C4/E4.
    //
    // Defense in depth without IAM: workspace-path addressing supplies the
    // trusted platform scope directly, and each resource aggregate enforces its
    // own persisted owner. Agent and Profile ids are Workspace-scoped identities,
    // while credential references are revalidated at publication. This proves the
    // resource service does not depend on a PEP-side process-local owner index.
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
      workspace_id: 'forged-body-owner',
      primary: {
        target: { model_id: MODEL },
        credential_binding: { type: 'exact', credential_source_id: ownedId },
      },
      disabled_endpoint_ids: [],
    };
    assert.equal((await req(base, 'PUT', scoped(WS_A, 'inference-profiles/owned-profile'), profile)).status, 200);
    const agentMcp = {
      name: 'owned MCP Agent',
      system: 'Use the owned MCP endpoint.',
      model: {
        mode: 'pinned',
        provider_identity_ref: 'default',
        model_ref: MODEL,
        backend_ref: 'default',
      },
      mcp_servers: [{
        name: 'owned',
        url: 'https://mcp.example.invalid/',
        credential: { id: ownedId, revision: ownedCredential.json.version },
      }],
    };
    assert.equal((await req(base, 'PUT', scoped(WS_A, 'agents/owned-agent'), agentMcp)).status, 200);

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
    const owningAgent = await req(base, 'GET', scoped(WS_A, 'agents/owned-agent'));
    assert.equal(owningAgent.status, 200);
    assert.deepEqual(
      owningAgent.json.mcp_servers[0].credential,
      { id: ownedId, revision: ownedCredential.json.version },
    );

    for (const uri of [
      scoped(WS_B, `credentials/${ownedId}`),
      scoped(WS_B, 'credential-pools/owned-pool'),
      scoped(WS_B, 'credential-pools/owned-pool/eligible'),
      scoped(WS_B, 'inference-profiles/owned-profile'),
      scoped(WS_B, 'agents/owned-agent'),
    ]) assert.equal((await req(base, 'GET', uri)).status, 404, `${uri} hides foreign ownership`);
    assert.equal((await req(base, 'PUT', scoped(WS_B, 'credential-pools/owned-pool'), pool)).status, 404);
    const ownedCredentialB = await req(base, 'POST', scoped(WS_B, 'credentials'), {
      workspace_id: 'forged-body-owner', kind: 'vault', provider_id: 'anthropic',
      env_key: null, secret: FAKE_KEY,
    });
    assert.equal(ownedCredentialB.status, 201, JSON.stringify(ownedCredentialB.json));
    const providerConnectionB = await req(base, 'POST', scoped(WS_B, 'provider-connections'), {
      idempotency_key: 'awaken-cli-e2e-workspace-b-provider',
      workspace_id: 'forged-body-owner',
      provider_id: 'anthropic',
      display_name: 'Anthropic B',
      dialect: 'anthropic_messages',
      base_url: `${upstream.url}/v1/`,
      timeout_secs: 300,
      credential_source_id: ownedCredentialB.json.id,
    });
    assert.equal(providerConnectionB.status, 201, JSON.stringify(providerConnectionB.json));
    const profileB = {
      ...profile,
      primary: {
        ...profile.primary,
        credential_binding: { type: 'exact', credential_source_id: ownedCredentialB.json.id },
      },
    };
    assert.equal((await req(base, 'PUT', scoped(WS_B, 'inference-profiles/owned-profile'), profileB)).status, 200);
    const storedProfileB = await req(base, 'GET', scoped(WS_B, 'inference-profiles/owned-profile'));
    assert.equal(storedProfileB.status, 200);
    assert.equal(storedProfileB.json.workspace_id, WS_B);
    assert.equal(
      storedProfileB.json.primary.credential_binding.credential_source_id,
      ownedCredentialB.json.id,
    );
    const storedProfileA = await req(base, 'GET', scoped(WS_A, 'inference-profiles/owned-profile'));
    assert.equal(storedProfileA.json.primary.credential_binding.credential_source_id, ownedId);
    const agentMcpB = { ...agentMcp, model: { id: MODEL } };
    assert.equal(
      (await req(base, 'PUT', scoped(WS_B, 'agents/owned-agent'), agentMcpB)).status,
      200,
      'same logical Agent id creates an independent B-scoped draft',
    );
    assert.equal((await req(base, 'GET', scoped(WS_B, 'agents/owned-agent'))).status, 200);
    const rejectedForeignCredential = await req(
      base, 'POST', scoped(WS_B, 'agents/owned-agent/publish'),
    );
    assert.equal(rejectedForeignCredential.status, 409, JSON.stringify(rejectedForeignCredential.json));
    // Error-projection cause/FMECA rule: foreign credential + B-scoped publish
    // -> stable RFC9457 code/type/detail. Reading the removed ad-hoc `error`
    // property turns a correct fail-closed response into a false E2E failure.
    assert.equal(rejectedForeignCredential.json.code, 'agent_publication_unresolvable');
    assert.equal(
      rejectedForeignCredential.json.type,
      'https://awaken.dev/problems/agent_publication_unresolvable',
    );
    assert.match(
      rejectedForeignCredential.json.detail,
      /credential is unavailable in this Workspace/,
      'B cannot publish a draft pinned to A credential',
    );
    const unchangedAgentA = await req(base, 'GET', scoped(WS_A, 'agents/owned-agent'));
    assert.equal(unchangedAgentA.status, 200);
    assert.deepEqual(unchangedAgentA.json.mcp_servers[0].credential, { id: ownedId, revision: ownedCredential.json.version });
    assert.equal((await req(base, 'POST', scoped(WS_B, 'inference-profiles/owned-profile/resolve-candidates'), {
      workspace_id: WS_A,
    })).status, 200, 'trusted B path resolves B profile despite forged body Workspace');
    const listedAgents = await req(base, 'GET', scoped(WS_B, 'agents'));
    assert.equal(listedAgents.status, 200);
    assert.ok(listedAgents.json.data.some((entry) => entry.id === 'owned-agent'));

    const uploaded = await fetch(`${base}/v1/workspaces/${WS_A}/files`, {
      method: 'POST',
      body: managedFileUploadForm('workspace-owned-file', 'owned.txt'),
    });
    assert.equal(uploaded.status, 200);
    const fileId = (await uploaded.json()).id;
    assert.equal((await fetch(`${base}/v1/workspaces/${WS_A}/files/${fileId}`)).status, 200);
    assert.equal((await fetch(`${base}/v1/workspaces/${WS_B}/files/${fileId}`)).status, 404);
    console.log('ok: config resources and files enforce intrinsic workspace ownership without IAM');

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
