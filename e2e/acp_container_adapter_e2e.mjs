// Deterministic container rows for the two npm-adapter ACP runtimes.
//
// codex_acp_live_e2e.mjs owns ten profile/runtime rows. Host, standalone-Worker
// and Native rows each have a deterministic fixture peer; the two container rows
// (Codex, Claude) only ran live against an operator-deployed server. This spec is
// their fixture: the aggregated `awaken` binary selects the Docker tier, the
// canonical sandbox image runs its `awaken-sandbox acp` bridge, and the catalog
// container argv (`claude-agent-acp`, `codex-acp`) resolves to
// fixtures/acp_container_adapter_fixture.mjs instead of the pinned npm adapters.
// Everything else (capability negotiation, publication, credential projection,
// Session launch and Managed transcript) is production.
//
// Cause/effect graph. C1 Docker is reachable; C2 the deployment lists the
// runtime and its container capability is negotiated; C3 a publication pins the
// runtime to a provider connection entered through the config API; C4 one
// prompt carries a unique marker. E1 a new awaken.sandbox container runs the
// adapter; E2 the reply carries the marker and the catalog delivery of that
// runtime (Claude: Anthropic process env; Codex: provider config plus an
// auth.json-only key); E3 the thread goes running -> idle with end_turn; E4
// deleting the Session removes its container.
//
// | Rule | C1 | C2 | C3 | C4 | Effect |
// | K1   | N  | -  | -  | -  | fail: the container tier is the subject |
// | K2   | Y  | Y  | Y  | Y  | E1 + E2 + E3 + E4 for claude and for codex |

import assert from 'node:assert/strict';
import { randomUUID } from 'node:crypto';
import fs from 'node:fs';
import http from 'node:http';
import net from 'node:net';
import os from 'node:os';
import path from 'node:path';
import { execFileSync, spawn, spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import Anthropic from '@anthropic-ai/sdk';
import { waitForVerifiedAcpCapability } from './fixtures/acp_capability.mjs';
import { ensureCanonicalSandboxImage } from './fixtures/sandbox_image.mjs';
import { closeHttpServer } from './http_server.mjs';
import { automatedAllInOneArgs } from './awaken_cli_args.mjs';
import { AWAKEN_BIN_ENV, cargoExecutable } from './cargo_binary.mjs';
import { initializeE2EInstallation, waitForSessionEventReceipt, waitForValue } from './harness.mjs';

const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const PORT = Number(process.env.E2E_PORT ?? 38517);
const BASE = `http://127.0.0.1:${PORT}`;
const TMP = fs.mkdtempSync(path.join(os.tmpdir(), 'awaken-acp-container-adapter-'));
const STORAGE = path.join(TMP, 'storage');
const IMAGE = `awaken-acp-container-adapter-e2e:${process.pid}`;
const BASE_IMAGE = process.env.AWAKEN_TEST_SESSION_IMAGE ?? 'awaken-sandbox:session-e2e';
const BETAS = ['managed-agents-2026-04-01'];
const SEAL_KEY = '00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff';
const FIXTURE = path.join(ROOT, 'e2e', 'fixtures', 'acp_container_adapter_fixture.mjs');

// One row per npm-adapter runtime: its catalog container argv, the provider
// dialect its row consumes, and the secret entered through the config API.
const ROWS = Object.freeze([
  Object.freeze({
    runtime: 'claude',
    executable: 'claude-agent-acp',
    provider: 'anthropic',
    dialect: 'anthropic_messages',
    model: 'claude-container-upstream',
    route: 'anthropic',
    secret: 'claude-container-key', // awaken-allow(secret): (fixture)
  }),
  Object.freeze({
    runtime: 'codex',
    executable: 'codex-acp',
    provider: 'openai',
    dialect: 'open_ai_responses',
    model: 'codex-container-upstream',
    route: 'openai',
    secret: 'codex-container-key', // awaken-allow(secret): (fixture)
  }),
]);

const dockerAvailable = () => spawnSync('docker', ['version'], { stdio: 'ignore' }).status === 0;

function sandboxContainerIds() {
  const result = spawnSync('docker', ['ps', '--all', '--quiet', '--filter', 'label=awaken.sandbox=1'],
    { encoding: 'utf8' });
  assert.equal(result.status, 0, result.stderr);
  return new Set(result.stdout.trim().split(/\s+/u).filter(Boolean));
}

function buildFixtureImage() {
  ensureCanonicalSandboxImage({ engine: 'docker', image: BASE_IMAGE, repoRoot: ROOT, timeoutMs: 3_600_000 });
  const context = path.join(TMP, 'image');
  fs.mkdirSync(context, { recursive: true });
  fs.copyFileSync(FIXTURE, path.join(context, 'acp_container_adapter_fixture.mjs'));
  // Each adapter executable is the catalog's bare container argv on the image
  // PATH, exactly where the pinned npm package would install its bin.
  const wrappers = ROWS.map(({ runtime, executable }) => {
    fs.writeFileSync(path.join(context, executable),
      `#!/bin/sh\nexec node /usr/local/lib/awaken-fixture/acp_container_adapter_fixture.mjs ${runtime} "$@"\n`);
    return `COPY ${executable} /usr/local/bin/${executable}\nRUN chmod 0555 /usr/local/bin/${executable}`;
  });
  fs.writeFileSync(path.join(context, 'Dockerfile'), [
    `FROM ${BASE_IMAGE}`,
    'USER root',
    'COPY acp_container_adapter_fixture.mjs /usr/local/lib/awaken-fixture/acp_container_adapter_fixture.mjs',
    ...wrappers,
    'USER 10001',
    '',
  ].join('\n'));
  // The default builder shares the daemon-local canonical base image.
  execFileSync('docker', ['buildx', 'build', '--builder', 'default', '--load', '--quiet', '--tag', IMAGE, context],
    { cwd: ROOT, stdio: ['ignore', 'ignore', 'inherit'] });
}

async function ready(child) {
  const listening = () => new Promise((resolve) => {
    const socket = net.createConnection({ host: '127.0.0.1', port: PORT });
    socket.once('connect', () => { socket.destroy(); resolve(true); });
    socket.once('error', () => { socket.destroy(); resolve(false); });
  });
  await waitForValue(async () => {
    if (child.exitCode !== null || child.signalCode !== null) {
      throw new Error(`awaken exited (${child.exitCode ?? child.signalCode}) before readiness`);
    }
    return listening();
  }, (connected) => connected, 'awaken to accept connections', { timeoutMs: 60_000 });
}

async function stop(child) {
  if (!child || child.exitCode !== null || child.signalCode !== null) return;
  const exited = new Promise((resolve) => child.once('exit', resolve));
  child.kill('SIGINT');
  await exited;
}

async function request(method, route, body) {
  const response = await fetch(`${BASE}${route}`, {
    method,
    headers: body === undefined ? {} : { 'content-type': 'application/json' },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  const value = await response.json().catch(() => ({}));
  assert.ok(response.ok, `${method} ${route}: ${response.status} ${JSON.stringify(value)}`);
  return value;
}

// The provider model directory the connection command consults. Inference is
// never requested: the adapter fixture reports projection and stops.
async function startModelDirectory() {
  const server = http.createServer((incoming, response) => {
    const row = ROWS.find(({ route }) => incoming.url?.startsWith(`/${route}/`));
    response.writeHead(row ? 200 : 404, { 'content-type': 'application/json' });
    response.end(JSON.stringify(row ? { data: [{ id: row.model }], has_more: false } : {}));
  });
  await new Promise((resolve, reject) => {
    server.once('error', reject);
    server.listen(0, '127.0.0.1', resolve);
  });
  const { port } = server.address();
  return { url: `http://127.0.0.1:${port}`, close: () => closeHttpServer(server) };
}

async function publish(row, workspace, directoryUrl) {
  await request('POST', '/v1/config/provider-connections', {
    idempotency_key: `acp-container-adapter-${row.runtime}`,
    workspace_id: workspace,
    provider_id: row.provider,
    display_name: row.provider,
    dialect: row.dialect,
    base_url: `${directoryUrl}/${row.route}/v1/`,
    timeout_secs: 30,
    secret: row.secret,
  });
  const agent = `container-adapter-${row.runtime}`;
  await request('PUT', `/v1/config/agents/${agent}`, {
    name: agent,
    model: `${row.model};provider=${row.provider};api=${row.dialect};executor=acp:${row.runtime}`,
    system: 'Exercise the container ACP adapter row.',
    tools: [],
  });
  await request('POST', `/v1/config/agents/${agent}/publish`);
  return agent;
}

function facts(text) {
  return Object.fromEntries(text.split(/\s+/u).slice(1).map((pair) => {
    const split = pair.indexOf('=');
    return [pair.slice(0, split), pair.slice(split + 1)];
  }));
}

async function runRow(client, row, agent, environmentId, directoryUrl) {
  const marker = `AWAKEN_CONTAINER_ADAPTER_${row.runtime}_${randomUUID().replaceAll('-', '')}`;
  const before = sandboxContainerIds();
  const session = await client.beta.sessions.create({ agent, environment_id: environmentId, betas: BETAS });
  try {
    const receipt = (await client.beta.sessions.events.send(session.id, {
      events: [{ type: 'user.message', content: [{ type: 'text', text: `Reply with ${marker}` }] }],
      betas: BETAS,
    })).data[0];
    assert.equal(receipt?.type, 'user.message', `K2 ${row.runtime}: exact User receipt`);
    const { delta } = await waitForSessionEventReceipt(client, session.id, receipt.id, BETAS,
      ({ delta: events }) => events.some((event) => event.type === 'session.error')
        || (events.some((event) => event.type === 'session.status_idle'
          && event.stop_reason?.type === 'end_turn')
          && events.some((event) => event.type === 'session.thread_status_idle')),
      `${row.runtime} container adapter reply and running-to-idle lifecycle`,
      { timeoutMs: 120_000 });
    const errors = delta.filter((event) => event.type === 'session.error');
    assert.deepEqual(errors, [], `K2 ${row.runtime}: the container run reports no error`);
    assert.ok(delta.some((event) => event.type === 'session.thread_status_running'),
      `E3 ${row.runtime}: thread entered running: ${delta.map((event) => event.type)}`);
    const reply = delta.filter((event) => event.type === 'agent.message')
      .flatMap((event) => event.content ?? []).map((block) => block.text ?? '')
      .find((text) => text.startsWith('CONTAINER_ADAPTER '));
    assert.ok(reply, `E2 ${row.runtime}: the adapter fixture answered: ${JSON.stringify(delta)}`);
    const seen = facts(reply);
    assert.equal(seen.role, row.runtime, `E2 the catalog argv selected the ${row.runtime} adapter`);
    assert.equal(seen.marker, marker, `E2 ${row.runtime}: this prompt reached the adapter`);
    assert.equal(seen.uid, '10001', `E1 ${row.runtime}: the adapter runs as the sandbox user`);
    assert.equal(seen.home, '/workspace/.acp-config', `E2 ${row.runtime}: Session-isolated config home`);
    assert.equal(seen.base, `${directoryUrl}/${row.route}/v1/`, `E2 ${row.runtime}: published endpoint`);
    assert.equal(seen.model, row.model, `E2 ${row.runtime}: published model`);
    assert.equal(seen.key, row.secret.slice(0, 8), `E2 ${row.runtime}: the entered credential`);
    if (row.runtime === 'claude') {
      assert.equal(seen.config, '/workspace/.acp-config', 'E2 claude: CLAUDE_CONFIG_DIR is the config home');
    } else {
      assert.equal(seen.wire, 'responses', 'E2 codex: Responses wire in CODEX_CONFIG');
      assert.equal(seen.envkey, 'absent', 'E2 codex: the key lives only in auth.json, not the process env');
    }
    const created = [...sandboxContainerIds()].filter((id) => !before.has(id));
    assert.ok(created.length > 0, `E1 ${row.runtime}: a new awaken.sandbox container ran the adapter`);
    // E4: deleting the Session disposes its sandbox. Stopping the server before
    // that settles would strand a live container (sandboxes survive restarts).
    await client.beta.sessions.delete(session.id, { betas: BETAS });
    await waitForValue(() => {
      const live = sandboxContainerIds();
      return created.filter((id) => live.has(id));
    }, (remaining) => remaining.length === 0, `E4 ${row.runtime}: Session delete to dispose its container`,
    { timeoutMs: 60_000, pollMs: 250 });
    console.log(`CONTAINER ADAPTER PASS: ${row.runtime} ${session.id} in ${created.join(',')}`);
  } catch (error) {
    await client.beta.sessions.delete(session.id, { betas: BETAS }).catch(() => {});
    throw error;
  }
}

async function main() {
  if (!dockerAvailable()) {
    fs.rmSync(TMP, { recursive: true, force: true });
    // These rows ARE the container tier; without Docker there is nothing to prove.
    throw new Error('K1 the container adapter rows require a reachable Docker daemon');
  }
  buildFixtureImage();
  fs.mkdirSync(STORAGE, { recursive: true });
  const binary = cargoExecutable({
    cwd: ROOT,
    packageName: 'awaken-cli',
    targetName: 'awaken',
    features: ['container-docker'],
    prebuiltEnvironmentName: AWAKEN_BIN_ENV,
    // Another E2E may rebuild target/debug/awaken without this feature before
    // spawn; launch an exact copy of the bytes this build produced.
    snapshotDestination: path.join(TMP, 'bin', 'awaken'),
  });
  const directory = await startModelDirectory();
  const configPath = path.join(TMP, 'config.toml');
  fs.writeFileSync(configPath, [
    `data_dir = ${JSON.stringify(STORAGE)}`,
    `bind = ${JSON.stringify(`127.0.0.1:${PORT}`)}`,
    `control_seal_key = ${JSON.stringify(SEAL_KEY)}`,
    'identity_mode = "no-login"',
    'sandbox_tier = "docker"',
    `container_image = ${JSON.stringify(IMAGE)}`,
    `acp_clis = ${JSON.stringify(ROWS.map(({ runtime }) => runtime))}`,
  ].join('\n'));
  const environment = { ...process.env };
  for (const key of ['AWAKEN_ACP_ARGV', 'AWAKEN_ACP_GATEWAY_URL', 'AWAKEN_ACP_LEASE_TOKEN']) delete environment[key];
  initializeE2EInstallation(environment, { binary, configPath });
  const server = spawn(binary, automatedAllInOneArgs('--config', configPath),
    { env: environment, stdio: ['ignore', 'ignore', 'inherit'] });
  try {
    await ready(server);
    for (const { runtime } of ROWS) await waitForVerifiedAcpCapability(BASE, runtime);
    const workspace = fs.readFileSync(path.join(STORAGE, 'platform-workspace-id'), 'utf8').trim();
    const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: BASE });
    const environmentResource = await client.beta.environments.create({
      name: `container-adapter-${process.pid}`,
      config: { type: 'cloud', networking: { type: 'unrestricted' } },
      betas: BETAS,
    });
    for (const row of ROWS) {
      const agent = await publish(row, workspace, directory.url);
      await runRow(client, row, agent, environmentResource.id, directory.url);
    }
    console.log('E2E PASS: container-tier Claude and Codex adapter rows ran through the production Docker launch.');
  } finally {
    await stop(server).catch(() => {});
    await directory.close();
    // A failed run can leave containers of this fixture image behind (for
    // example an interrupted capability probe); remove exactly those.
    const leftovers = spawnSync('docker', ['ps', '--all', '--quiet', '--filter', `ancestor=${IMAGE}`],
      { encoding: 'utf8' }).stdout?.trim().split(/\s+/u).filter(Boolean) ?? [];
    if (leftovers.length > 0) spawnSync('docker', ['rm', '--force', ...leftovers], { stdio: 'ignore' });
    spawnSync('docker', ['image', 'rm', '--force', IMAGE], { stdio: 'ignore' });
    fs.rmSync(TMP, { recursive: true, force: true });
  }
}

main().catch((error) => {
  console.error('E2E FAIL:', error);
  process.exitCode = 1;
});
