// T21-C04 (真实服务断连): Control and Coordinator run as SEPARATE processes on
// PostgreSQL, configured like the k3d distributed-control overlay: the shipped
// `awaken coordinator`, and the canonical split-Control assembly hosted by the
// scenario binary (AWAKEN_MODEL_MODE=distributed-control), which adds only the
// deterministic model-publication resolver. Control's
// `coordinator_internal_url` points at an in-test TCP proxy that can drop the
// response of a request the Coordinator already committed ("lost ack") and can
// cut connections. A route table (the k3d nginx map, reproduced in-process)
// fronts both public listeners for the official SDK.
//
// | Row | Fault | Expected |
// |---|---|---|
// | N1 | the Coordinator's register response is dropped after it committed | Control replays the original intent; exactly one healthcheck Work |
// | N2 | the Coordinator is hard-killed and respawned while the proxy cuts connections | the replayed registration adds nothing; one healthcheck Work |
// | N3 | a Session pinned to the Environment, then the Environment is updated and archived | the Session keeps its frozen Environment pin |
// | N4 | a new Session on the archived Environment | refused (new admission obeys the archive) |
// | NC | an in-process registrar-stub observation (no proxy traffic, one pid) | the cross-process checker rejects it |
//
// Oracles (groups-5): O0 the old Session keeps its frozen version; O1 new
// admission obeys the archive; O2 the original intent is restored with no
// second healthcheck/Work (measured before the archive, which withdraws the
// healthcheck Work by design).
//
// Not accepted here (needs a third, Worker-role process with Worker
// credentials): the pinned Session EXECUTING a Run after the archive. Split
// Control/Coordinator never own a local claim pool (`run_local_pool` is
// all-in-one only), so without a Worker the Session can be created and observed
// but not driven.
//
// Run: build the production binary (harness ensureProductionBuilt / AWAKEN_BIN),
// psql on PATH, then
//   AWAKEN_TEST_DATABASE_URL=postgres://... AWAKEN_TEST_REQUIRE_POSTGRES=1 \
//   E2E_PORT=<free base port; uses +0..+6> node e2e/control_coordinator_disconnect_postgres_e2e.mjs
import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { randomBytes } from 'node:crypto';
import fs from 'node:fs';
import http from 'node:http';
import net from 'node:net';
import os from 'node:os';
import path from 'node:path';
import Anthropic from '@anthropic-ai/sdk';
import {
  ensureProductionBuilt, initializeE2EInstallation, hardKillServer, stopServer, waitForPort, waitForValue,
  cleanupFixtureTree, pass,
} from './harness.mjs';
import { postgresQuery, postgresRows } from './postgres.mjs';
import { withoutAmbientProviderKeys } from './process_environment.mjs';
import { cargoScenarioHostBundle } from './cargo_binary.mjs';

const BETAS = ['managed-agents-2026-04-01'];
const BASE_PORT = Number(process.env.E2E_PORT ?? 38640);
const PORTS = {
  controlPublic: BASE_PORT,
  controlInternal: BASE_PORT + 1,
  coordinatorPublic: BASE_PORT + 2,
  coordinatorInternal: BASE_PORT + 3,
  internalProxy: BASE_PORT + 4,
  router: BASE_PORT + 5,
};
const WORKSPACE = 'workspace-t21c04';
// The one offering the distributed-control publication resolver serves.
const CONTROL_SCENARIO_MODEL = 'adr71-echo';
const SEAL_KEY = '00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff';
const REGISTER_PATH = '/internal/v1/executable-environments/register';
const ROUTER = `http://127.0.0.1:${PORTS.router}`;
const REPO_ROOT = path.resolve(import.meta.dirname, '..');

// ---- PostgreSQL fixture ------------------------------------------------------
function postgresFixtureUrl() {
  const required = process.env.AWAKEN_TEST_REQUIRE_POSTGRES === '1';
  const url = process.env.AWAKEN_TEST_DATABASE_URL?.trim();
  if (!url && required) throw new Error('AWAKEN_TEST_REQUIRE_POSTGRES=1 but AWAKEN_TEST_DATABASE_URL is not configured');
  return url || null;
}

function ownedDatabase(adminUrl) {
  const name = `awaken_t21c04_${process.pid}_${Date.now().toString(36)}`;
  postgresQuery({ url: adminUrl }, `CREATE DATABASE ${name};`);
  const url = new URL(adminUrl);
  url.pathname = `/${name}`;
  return { url: url.toString(), drop: () => postgresQuery({ url: adminUrl }, `DROP DATABASE IF EXISTS ${name} WITH (FORCE);`) };
}

function healthchecks(database, environmentId) {
  const rows = postgresRows({ url: database.url },
    "SELECT work_id FROM work_queue_item WHERE environment_id = ? AND data_type = 'healthcheck'", environmentId);
  return rows.length;
}

// ---- the internal TCP proxy (Control -> Coordinator) -------------------------
function startInternalProxy(target) {
  const state = { mode: 'pass', dropRegisterAcks: 0, requests: [], dropped: 0, cut: 0, controlPeers: new Set() };
  const server = net.createServer((client) => {
    if (state.mode === 'cut') {
      state.cut += 1;
      client.destroy();
      return;
    }
    state.controlPeers.add(client.remotePort);
    const upstream = net.connect(target, '127.0.0.1');
    let dropNextResponse = false;
    client.on('data', (chunk) => {
      for (const match of chunk.toString('latin1').matchAll(/^(GET|POST|PUT|DELETE) (\S+) HTTP\/1\.1/gmu)) {
        state.requests.push(match[2]);
        if (match[2].startsWith(REGISTER_PATH) && state.dropRegisterAcks > 0) {
          state.dropRegisterAcks -= 1;
          dropNextResponse = true;
        }
      }
      upstream.write(chunk);
    });
    upstream.on('data', (chunk) => {
      if (dropNextResponse) {
        // The Coordinator already answered (it committed); the answer is lost.
        state.dropped += 1;
        client.destroy();
        upstream.destroy();
        return;
      }
      client.write(chunk);
    });
    const close = () => { client.destroy(); upstream.destroy(); };
    client.on('error', close); upstream.on('error', close);
    client.on('close', close); upstream.on('close', close);
  });
  return new Promise((resolve) => server.listen(PORTS.internalProxy, '127.0.0.1', () => resolve({ server, state })));
}

// ---- the public route table (k3d awaken-api route map) ------------------------
function startRouter() {
  const coordinatorRoute = /^\/v1\/(sessions|deployments|deployment_runs|files|memory_stores|skills|models|dreams|ai-sdk|ag-ui|a2a|message|capabilities)(\/|$|\?)|^\/v1\/environments\/[^/]+\/work(\/|$|\?)/u;
  const server = http.createServer((request, response) => {
    if (request.url.startsWith('/internal/') || request.url.startsWith('/v1/worker/')) {
      response.writeHead(404).end();
      return;
    }
    const port = coordinatorRoute.test(request.url) ? PORTS.coordinatorPublic : PORTS.controlPublic;
    const upstream = http.request({ host: '127.0.0.1', port, method: request.method, path: request.url, headers: request.headers }, (reply) => {
      response.writeHead(reply.statusCode, reply.headers);
      reply.pipe(response);
    });
    upstream.on('error', () => { if (!response.headersSent) response.writeHead(502); response.end(); });
    request.pipe(upstream);
  });
  return new Promise((resolve) => server.listen(PORTS.router, '127.0.0.1', () => resolve(server)));
}

// ---- process configuration ------------------------------------------------------
function writeConfig(root, role, database) {
  const dir = path.join(root, role);
  fs.mkdirSync(path.join(dir, 'home', '.awaken'), { recursive: true });
  const token = path.join(root, 'service-token');
  const registration = path.join(root, 'registration-token');
  const common = [
    `data_dir = ${JSON.stringify(path.join(dir, 'data'))}`,
    `expected_platform_workspace_id = ${JSON.stringify(WORKSPACE)}`,
    'mode = "server"',
    `role = ${JSON.stringify(role)}`,
    'no_browser = true',
    'identity_mode = "no-login"',
    'log_filter = "warn"',
    `control_service_token_file = ${JSON.stringify(token)}`,
    `executable_agent_registration_token_file = ${JSON.stringify(registration)}`,
  ];
  const specific = role === 'control' ? [
    `bind = "127.0.0.1:${PORTS.controlPublic}"`,
    `internal_bind = "127.0.0.1:${PORTS.controlInternal}"`,
    `coordinator_internal_url = "http://127.0.0.1:${PORTS.internalProxy}"`,
    `control_seal_key = ${JSON.stringify(SEAL_KEY)}`,
  ] : [
    `bind = "127.0.0.1:${PORTS.coordinatorPublic}"`,
    `internal_bind = "127.0.0.1:${PORTS.coordinatorInternal}"`,
    `control_internal_url = "http://127.0.0.1:${PORTS.controlInternal}"`,
    `runtime_database_url = ${JSON.stringify(database.url)}`,
    `resource_database_url = ${JSON.stringify(database.url)}`,
    `sessions_db = ${JSON.stringify(database.url)}`,
    `captured_content_db = ${JSON.stringify(database.url)}`,
    // Server-mode Coordinators authenticate Worker requests with signed
    // credentials (k3d binds them through worker_kubernetes; a host
    // installation projects a trust file). No Worker runs here, but the
    // production boot refuses a Coordinator without a Worker trust root.
    `worker_trust_credentials_file = ${JSON.stringify(path.join(root, 'worker-trust.json'))}`,
    'dispatch_owner = "t21c04-coordinator"',
    'sandbox_tier = "local"',
    'acp_clis = []',
  ];
  const configPath = path.join(dir, 'config.toml');
  fs.writeFileSync(configPath, [...common, ...specific].join('\n'));
  const env = { ...withoutAmbientProviderKeys(process.env), HOME: path.join(dir, 'home'), USERPROFILE: path.join(dir, 'home') };
  return { configPath, env, cwd: dir };
}

// The k3d distributed-control Pod runs Control through the scenario binary in
// AWAKEN_MODEL_MODE=distributed-control: it loads the same control.toml and
// assembles the canonical split-Control application, adding only the
// deterministic model-publication resolver. The shipped `awaken control`
// exposes no tenant model supply (BYOK needs a closed-deployment credential
// executor: POST /v1/config/provider-connections -> 403 model_supply_managed),
// so it cannot publish the Agent the Session rows need. Coordinator stays the
// shipped `awaken coordinator`.
function spawnControl(scenarioHost, config) {
  return spawn(scenarioHost, [], {
    cwd: config.cwd,
    env: {
      ...config.env,
      AWAKEN_MODEL_MODE: 'distributed-control',
      AWAKEN_SCENARIO_CONFIG: config.configPath,
      AWAKEN_HTTP_ADDR: `127.0.0.1:${PORTS.controlPublic}`,
    },
    stdio: ['ignore', 'inherit', 'inherit'],
  });
}

function spawnRole(binary, role, config) {
  const child = spawn(binary, [role, '--config', config.configPath], {
    cwd: config.cwd, env: config.env, stdio: ['ignore', 'inherit', 'inherit'],
  });
  return child;
}

// ---- the cross-process checker (O2 evidence) ------------------------------------
function crossProcessViolations(evidence) {
  const violations = [];
  if (evidence.controlPid === evidence.coordinatorPid || evidence.controlPid === process.pid) {
    violations.push('NC Control and Coordinator are not separate OS processes');
  }
  if (evidence.registerRequests < 2) violations.push(`O2 the registration did not cross the network twice (${evidence.registerRequests})`);
  if (evidence.droppedAcks < 1) violations.push('O2 no committed response was lost on the wire');
  if (evidence.durableHealthchecks !== 1) violations.push(`O2 ${evidence.durableHealthchecks} durable healthcheck Work rows`);
  return violations;
}

let control; let coordinator; let proxy; let router; let database;
const root = fs.mkdtempSync(path.join(os.tmpdir(), 'awaken-t21c04-'));

async function main() {
  const adminUrl = postgresFixtureUrl();
  if (!adminUrl) {
    console.log('SKIPPED PostgreSQL fixture is not configured (set AWAKEN_TEST_DATABASE_URL)');
    return;
  }
  database = ownedDatabase(adminUrl);
  fs.writeFileSync(path.join(root, 'service-token'), 't21c04-service-token\n', { mode: 0o600 });
  fs.writeFileSync(path.join(root, 'registration-token'), 't21c04-registration-token\n', { mode: 0o600 });
  fs.writeFileSync(path.join(root, 'worker-trust.json'), JSON.stringify([{
    worker_id: 't21c04-worker', key_id: 't21c04-worker-key', credential_id: 't21c04-worker-request',
    secret_base64: randomBytes(32).toString('base64'),
  }]), { mode: 0o600 });
  const binary = ensureProductionBuilt();
  const coordinatorConfig = writeConfig(root, 'coordinator', database);
  const controlConfig = writeConfig(root, 'control', database);
  for (const config of [coordinatorConfig, controlConfig]) {
    initializeE2EInstallation(config.env, { binary, cwd: config.cwd, configPath: config.configPath });
  }
  proxy = await startInternalProxy(PORTS.coordinatorInternal);
  router = await startRouter();
  coordinator = spawnRole(binary, 'coordinator', coordinatorConfig);
  await waitForPort(PORTS.coordinatorPublic, 180_000, coordinator);
  control = spawnControl(cargoScenarioHostBundle({ cwd: REPO_ROOT, environment: process.env }).scenarioHost, controlConfig);
  await waitForPort(PORTS.controlPublic, 180_000, control);

  const client = new Anthropic({ apiKey: 'e2e-local', baseURL: ROUTER, maxRetries: 0 }); // awaken-allow(secret): (local no-login placeholder)

  // N1: the first registration answer is lost after the Coordinator committed.
  proxy.state.dropRegisterAcks = 1;
  const environment = await client.beta.environments.create({ name: 't21c04-env', config: { type: 'self_hosted' }, betas: BETAS });
  await waitForValue(
    () => ({ register: proxy.state.requests.filter((p) => p.startsWith(REGISTER_PATH)).length, dropped: proxy.state.dropped }),
    (seen) => seen.dropped >= 1 && seen.register >= 2,
    'N1 Control replays the registration revision after the lost committed answer',
    { timeoutMs: 120_000, pollMs: 250 },
  );
  assert.equal(healthchecks(database, environment.id), 1, 'N1/O2 one healthcheck Work after the replay');

  // N2: hard-kill the Coordinator, cut connections, respawn, and let the
  // outbox replay again through the restored path.
  const registersBefore = proxy.state.requests.filter((p) => p.startsWith(REGISTER_PATH)).length;
  proxy.state.mode = 'cut';
  await hardKillServer(coordinator, 'T21-C04 Coordinator restart');
  coordinator = spawnRole(binary, 'coordinator', coordinatorConfig);
  await waitForPort(PORTS.coordinatorPublic, 180_000, coordinator);
  proxy.state.mode = 'pass';
  const updated = await client.beta.environments.update(environment.id, { description: 'revision two', betas: BETAS });
  await waitForValue(
    () => proxy.state.requests.filter((p) => p.startsWith(REGISTER_PATH)).length,
    (count) => count > registersBefore,
    'N2 the revision-two registration crossed the restored path after the restart',
    { timeoutMs: 120_000, pollMs: 250 },
  );
  assert.equal(healthchecks(database, environment.id), 1, 'N2/O2 still one healthcheck Work after restart and replay');
  const work = await fetch(`${ROUTER}/v1/environments/${environment.id}/work`, { headers: { 'anthropic-beta': BETAS.join(','), 'x-api-key': 'e2e-local' } }); // awaken-allow(secret): (local no-login placeholder)
  assert.equal(work.status, 200, 'N2 the Coordinator serves the Environment work list');
  const listed = (await work.json()).data ?? [];
  assert.equal(listed.filter((item) => JSON.stringify(item).includes('healthcheck')).length, 1, 'N2/O2 one public healthcheck Work');

  // N3/N4: a pinned Session keeps its frozen pin; new admission obeys the archive.
  const agent = await client.beta.agents.create({ name: 't21c04-agent', model: CONTROL_SCENARIO_MODEL, system: 't21c04', betas: BETAS });
  const session = await client.beta.sessions.create({ agent: agent.id, environment_id: environment.id, betas: BETAS });
  const pinned = JSON.stringify(session.environment ?? { environment_id: session.environment_id });
  await client.beta.environments.update(environment.id, { description: 'revision three', betas: BETAS });
  // O2 is evaluated on the durable state after the replays and revision three,
  // before the archive: archiving withdraws the healthcheck Work by design.
  const durableHealthchecksBeforeArchive = healthchecks(database, environment.id);
  await client.beta.environments.archive(environment.id, { betas: BETAS });
  assert.equal(healthchecks(database, environment.id), 0, 'N3 the healthcheck Work is withdrawn by archive');
  const after = await client.beta.sessions.retrieve(session.id, { betas: BETAS });
  assert.equal(JSON.stringify(after.environment ?? { environment_id: after.environment_id }), pinned, 'N3/O0 the Session keeps its frozen Environment pin');
  await assert.rejects(
    client.beta.sessions.create({ agent: agent.id, environment_id: environment.id, betas: BETAS }),
    (error) => error?.status >= 400 && error?.status < 500,
    'N4/O1 a new Session on the archived Environment is refused',
  );

  const evidence = {
    controlPid: control.pid,
    coordinatorPid: coordinator.pid,
    registerRequests: proxy.state.requests.filter((p) => p.startsWith(REGISTER_PATH)).length,
    droppedAcks: proxy.state.dropped,
    durableHealthchecks: durableHealthchecksBeforeArchive,
  };
  assert.deepEqual(crossProcessViolations(evidence), [], 'T21-C04 cross-process evidence');
  assert.ok(updated.description === 'revision two', 'the revision-two update was applied by Control');
  pass(`T21-C04: lost ack and Coordinator restart keep one healthcheck; archive withdraws it (${JSON.stringify(evidence)})`);

  // NC: what an in-process registrar stub run can show is rejected.
  const stubbed = { controlPid: process.pid, coordinatorPid: process.pid, registerRequests: 0, droppedAcks: 0, durableHealthchecks: 1 };
  const violations = crossProcessViolations(stubbed);
  assert.ok(violations.some((v) => v.startsWith('NC')) && violations.some((v) => v.includes('cross the network')),
    `NC: a component registrar stub cannot stand in for the process boundary: ${violations}`);
  pass('T21-C04 NC: an in-process registrar stub observation is rejected');
}

try {
  await main();
  console.log('\nE2E PASS: T21-C04 Control/Coordinator disconnect on PostgreSQL');
} catch (error) {
  console.error(`E2E FAIL: ${error.stack}`);
  process.exitCode = 1;
} finally {
  if (control) await stopServer(control);
  if (coordinator) await stopServer(coordinator);
  if (proxy) proxy.server.close();
  if (router) router.close();
  if (database) database.drop();
  cleanupFixtureTree(root);
}
