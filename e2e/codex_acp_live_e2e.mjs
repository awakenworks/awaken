// ACP end-to-end release gate (AWAKEN_ACP_RUNTIME=codex|claude).
//
// One script owns live deployments and the deterministic native-executable peer:
//   CODEX_ACP_PROFILE=host-fixture node e2e/codex_acp_live_e2e.mjs
//   CODEX_ACP_PROFILE=worker-host-fixture node e2e/codex_acp_live_e2e.mjs
//   CODEX_ACP_PROFILE=native-fixture AWAKEN_ACP_RUNTIME=native node e2e/codex_acp_live_e2e.mjs
//   CODEX_ACP_PROFILE=native-host-fixture AWAKEN_ACP_RUNTIME=native node e2e/codex_acp_live_e2e.mjs
//
//   CODEX_ACP_LIVE=1 CODEX_ACP_PROFILE=host-login \
//     node e2e/codex_acp_live_e2e.mjs
//
//   CODEX_ACP_LIVE=1 CODEX_ACP_PROFILE=container \
//     CODEX_ACP_BASE_URL=http://127.0.0.1:38080 \
//     node e2e/codex_acp_live_e2e.mjs
//
// Cause/effect graph:
// C1 explicit live opt-in; C2 deployment profile is host-login or container;
// C3 Codex owns an available login; C4 Awaken publishes a BackendDefault Agent;
// C5 one real prompt completes. E1 the committed transcript contains the unique
// marker and thread running -> idle states. Host profiles additionally prove
// discovery, Worker capability and the shared resource mission; container proves a new
// managed sandbox was observed.
//
// Decision table:
// | Rule | C1 | C2 | C3-C5 | Effect |
// | H1 | Y | host-login | Y | E1 + live local capability + shared resource oracle |
// | D1 | - | host-fixture | Y | same resource oracle, no inference service |
// | W1 | - | worker-host-fixture | Y | bootstrap embedded source, remote-only
// |    |   |                     |   | restart, then signed standalone Worker |
// | N1 | - | native-host-fixture | Y | API-selected TrustedHost Native Hand, same resource oracle |
// | C1 | Y | container  | Y | E1 + newly observed managed container |
// | F1 | N | live profile | - | fail before invoking the adapter |
// | F2 | Y | other      | - | fail before server/session work |
// W1 cutover subrules: old embedded lease live => wait without admission;
// old lease expired + exact standalone Ready => one ACP claimer and the shared
// resource oracle; renewed embedded/foreign ACP or missing standalone => bounded
// failure. This uses Registry lease evidence, never a fixture delete or a 404.
// Linux H1/D1/N1 use the same scoped execution selection and namespace oracle;
// the exhaustive platform/profile/runtime rules live in acp_live_profile.test.mjs.
// Awaken discovers login through the CLI, without opening its authentication file.

import assert from 'node:assert/strict';
import { randomBytes } from 'node:crypto';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { spawn, spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import Anthropic from '@anthropic-ai/sdk';
import { waitForVerifiedAcpCapability } from './fixtures/acp_capability.mjs';
import { automatedAllInOneArgs } from './awaken_cli_args.mjs';
import { AWAKEN_BIN_ENV, cargoExecutable } from './cargo_binary.mjs';
import { availablePort, FAKE_KEY, startUpstream, initializeE2EInstallation, waitForSessionEventReceipt, stopServer as stop, hardKillServer, runCleanupSteps } from './harness.mjs';
import { installHostResourceFixture, verifyAgentResources } from './fixtures/trusted_host_resources.mjs';
import { resolveAcpLiveProfile } from './acp_live_profile.mjs';
import { assertWorkerLocalCredentialCustody } from './fixtures/worker_local_credentials.mjs';
import { prepareStandaloneHostWorker } from './fixtures/standalone_host_worker.mjs';

// One selection drives deployment, companion features AND the resource oracle.
// Real host login cannot silently fall back to unsupervised Linux execution.
const { profile, runtime, nativeHost, native, scopedHost, standaloneWorker } = resolveAcpLiveProfile({
  profile: process.env.CODEX_ACP_PROFILE,
  runtime: process.env.AWAKEN_ACP_RUNTIME,
  platform: process.platform,
  live: process.env.CODEX_ACP_LIVE,
});

const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const BETAS = ['managed-agents-2026-04-01'];
const marker = `CODEX-ACP-READY-${Date.now()}`;
const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

function managedContainerIds() {
  const result = spawnSync('docker', ['ps', '-q', '--filter', 'label=awaken.sandbox=1'], {
    encoding: 'utf8',
  });
  assert.equal(
    result.status,
    0,
    `Docker is required for this gate: ${result.stderr || result.error || ''}`,
  );
  return new Set(result.stdout.trim().split(/\s+/).filter(Boolean));
}

function awakenBin(snapshotDestination) {
  return cargoExecutable({
    cwd: ROOT,
    packageName: 'awaken-cli',
    targetName: 'awaken',
    snapshotDestination,
    // The resource mission uses a local bare Git remote. Reuse the existing
    // test-support transport aperture; production remains HTTPS-only. Do not
    // accept an unqualified prebuilt binary with unknown feature provenance.
    ...(profile === 'container' ? { prebuiltEnvironmentName: AWAKEN_BIN_ENV } : { features: ['test-support'] }),
  });
}

async function waitReady(child, baseURL, tokenPath) {
  const deadline = Date.now() + 180_000;
  while (Date.now() < deadline) {
    try {
      const apiKey = fs.existsSync(tokenPath) ? fs.readFileSync(tokenPath, 'utf8').trim() : '';
      const response = await fetch(`${baseURL}/v1/capabilities`, {
        headers: apiKey === '' ? {} : { 'x-api-key': apiKey },
      });
      if (response.ok) return apiKey;
    } catch {
      // Startup is still in progress.
    }
    if (child.exitCode !== null) {
      throw new Error(`awaken exited before readiness with ${child.exitCode}`);
    }
    await sleep(200);
  }
  throw new Error('awaken did not become ready');
}

async function request(baseURL, apiKey, method, route, body) {
  const response = await fetch(`${baseURL}${route}`, {
    method,
    headers: {
      'x-api-key': apiKey,
      ...(body === undefined ? {} : { 'content-type': 'application/json' }),
    },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  const value = await response.json().catch(() => ({}));
  return { response, value };
}

async function startHostLoginProfile() {
  const temp = fs.mkdtempSync(path.join(os.tmpdir(), 'awaken-acp-host-resource-'));
  let child;
  let upstream;
  let external;
  try {
  // Artifact race rule: another scenario may rebuild target/debug/awaken with
  // different features while doctor/initialization or a restart is pending.
  // Reuse the runner's copy snapshot so EVERY launch keeps the selected bytes.
  const binary = awakenBin(path.join(temp, 'product',
    process.platform === 'win32' ? 'awaken.exe' : 'awaken'));
  cargoExecutable({
    cwd: ROOT, packageName: 'awaken-sandbox', targetName: 'awaken-sandbox',
    // Scoped Repository I/O runs in this companion, not the Control binary.
    // Give only this fixture build the SAME local-Git test aperture; production
    // companion builds continue to reject anonymous non-HTTPS transports.
    features: ['hand', ...(scopedHost ? ['awaken-sandbox-local/test-support'] : [])],
    snapshotDestination: path.join(path.dirname(binary), process.platform === 'win32' ? 'awaken-sandbox.exe' : 'awaken-sandbox'),
  });
  const environment = profile === 'host-fixture' || standaloneWorker
    ? await installHostResourceFixture(ROOT, temp, path.join(temp, 'data'), runtime)
    : process.env;
  if (native) upstream = await startUpstream('gitRepo', { models: ['fake-haiku'] });
  if (!native) {
  const doctor = spawnSync(binary, ['doctor', 'acp', '--json'], {
    env: environment,
    encoding: 'utf8',
  });
  assert.equal(doctor.status, 0, doctor.stderr);
  const codex = JSON.parse(doctor.stdout).acp.find((row) => row.id === runtime);
  assert.equal(codex?.login_state, 'available', JSON.stringify(codex));
  }

  const port = await availablePort(0);
  const config = path.join(temp, 'config.toml');
  if (standaloneWorker) external = await prepareStandaloneHostWorker({
    root: ROOT, temp, productDirectory: path.dirname(binary), runtime, scopedHost,
  });
  const controlConfig = [
    `data_dir = ${JSON.stringify(path.join(temp, 'data'))}`,
    `sandbox_dir = ${JSON.stringify(path.join(temp, 'sandboxes'))}`,
    `bind = ${JSON.stringify(`127.0.0.1:${port}`)}`,
    // This profile reads the local bootstrap admin token; Cloud login is a
    // separate live gate and must not become an implicit prerequisite.
    'identity_mode = "self-managed"',
    `control_seal_key = ${JSON.stringify(randomBytes(32).toString('hex'))}`,
    // Host-login is the trusted local-process gate. Container isolation has its
    // own profile below, and native sandbox availability is host-specific.
    `sandbox_tier = "${native && !nativeHost ? 'namespace' : 'local'}"`,
    `sandbox_trusted_host_scoped_execution = ${scopedHost}`,
    'log_filter = "warn"',
    `acp_clis = ${JSON.stringify(native ? [] : [runtime])}`,
    ...(external?.controlConfig ?? []),
    // Select the runtime under test, without configuring a default backend or
    // credential material. Its login and executable remain discovery-owned.
  ];
  fs.writeFileSync(config, controlConfig.join('\n'));
  initializeE2EInstallation(environment, { binary, configPath: config });
  const launch = () => {
    const process = spawn(binary, automatedAllInOneArgs('--config', config), {
    env: environment,
    stdio: ['ignore', 'pipe', 'pipe'],
  });
    // Keep diagnostic warnings, but do not emit the installation setup token.
    for (const stream of [process.stdout, process.stderr]) {
      let pending = '';
      stream.on('data', (chunk) => {
        const lines = (pending + chunk.toString()).split('\n');
        pending = lines.pop();
        for (const line of lines) if (!/^\s*Setup\s/u.test(line)) console.error(line);
      });
    }
    return process;
  };
  child = launch();
  const baseURL = `http://127.0.0.1:${port}`;
  let credentialBaseline;
  const verifyCredentials = () => {
    // C1 this fresh installation selects one backend-owned login, not a managed
    // Native provider. C2 startup/restart/resource completion. C1+C2 => one
    // unchanged WorkerLocal locator, zero material refs and sealed secrets.
    // Native intentionally imports a fixture Vault key and is not this rule.
    if (!native) credentialBaseline = assertWorkerLocalCredentialCustody(
      path.join(temp, 'data', 'credential.db'), runtime, credentialBaseline,
    );
  };
    const apiKey = await waitReady(child, baseURL, path.join(temp, 'data', 'admin-token'));
    if (!native) await waitForVerifiedAcpCapability(baseURL, runtime, {
      timeoutMs: 60_000,
      pollMs: 200,
      requireAvailableLogin: true,
      apiKey,
    });
    verifyCredentials();

    if (external) {
      // W1 bootstrap authors the real locator once. The persistent source is
      // reused unchanged. The second launch uses the existing remote-only
      // AllInOne role, so it cannot start another embedded claim pool while
      // the standalone Worker takes over after the old lease expires.
      await stop(child);
      fs.writeFileSync(config, [...controlConfig.map((line) =>
        line.startsWith('acp_clis =') ? 'acp_clis = []' : line),
      'run_local_pool = false'].join('\n'));
      child = launch();
      await waitReady(child, baseURL, path.join(temp, 'data', 'admin-token'));
      await external.start(credentialBaseline);
      await external.verifyRevocation();
      await waitForVerifiedAcpCapability(baseURL, runtime, {
        timeoutMs: 60_000, pollMs: 200, requireAvailableLogin: true, apiKey,
      });
      verifyCredentials();
    }

    if (native) {
      // Native uses the production provider import and real Messages transport,
      // not a scenario-only route or a second fake provider implementation.
      const scope = await request(baseURL, apiKey, 'GET', '/v1/capabilities');
      const workspace = scope.response.headers.get('anthropic-workspace-id');
      assert.ok(workspace, 'provider import uses the authenticated Workspace response');
      const imported = await request(baseURL, apiKey, 'POST', '/v1/config/provider-connections', {
        idempotency_key: 'native-resource-provider', workspace_id: workspace,
        provider_id: 'anthropic', display_name: 'Native resource fixture',
        dialect: 'anthropic_messages', base_url: `${upstream.url}/v1/`,
        timeout_secs: 30, secret: FAKE_KEY,
      });
      assert.equal(imported.response.status, 201, JSON.stringify(imported.value));
    }

    const agent = process.env.AWAKEN_ACP_AGENT ?? `${runtime}-host-login-live`;
    let result = await request(baseURL, apiKey, 'PUT', `/v1/config/agents/${agent}`, {
      name: 'Real host Codex login',
      system: 'Return exactly the text requested by the user.',
      model: native ? { id: 'fake-haiku' } : { mode: 'backend_default', backend_ref: `acp:${runtime}` },
      tools: [],
    });
    assert.equal(result.response.status, 200, JSON.stringify(result.value));
    result = await request(baseURL, apiKey, 'POST', `/v1/config/agents/${agent}/publish`);
    assert.equal(result.response.status, 200, JSON.stringify(result.value));
    return { baseURL, apiKey, agent, temp, physicalRoot: external?.physicalRoot,
      verifyCredentials, restart: async ({ crash = false } = {}) => {
      if (external) {
        await external.restart({ crash });
        verifyCredentials();
        return;
      }
      if (crash) await hardKillServer(child, 'TrustedHost live Session');
      else await stop(child);
      child = launch();
      await waitReady(child, baseURL, path.join(temp, 'data', 'admin-token'));
      if (!native) await waitForVerifiedAcpCapability(baseURL, runtime, {
        timeoutMs: 60_000, pollMs: 200, requireAvailableLogin: true, apiKey,
      });
      verifyCredentials();
    }, cleanup: async () => {
      await runCleanupSteps([
        () => external?.cleanup(),
        () => stop(child),
        () => upstream?.close(),
      ]);
      fs.rmSync(temp, { recursive: true, force: true });
    } };
  } catch (error) {
    // Preserve startup failure and logs, while still releasing every owner.
    await runCleanupSteps([
      () => external?.cleanup(),
      () => child && stop(child),
      () => upstream?.close(),
    ], [error]);
  }
}

const deployment = profile !== 'container'
  ? await startHostLoginProfile()
  : {
      baseURL: process.env.CODEX_ACP_BASE_URL ?? 'http://127.0.0.1:38080',
      apiKey: process.env.AWAKEN_API_KEY ?? 'local-live-gate',
      agent: process.env.AWAKEN_ACP_AGENT ?? 'codex',
      cleanup: async () => {},
    };

const client = new Anthropic({
  apiKey: deployment.apiKey,
  baseURL: deployment.baseURL,
});

const session = await client.beta.sessions.create({
  agent: deployment.agent,
  environment_id: 'env_local',
  betas: BETAS,
});

const baselineContainers = profile === 'container' ? managedContainerIds() : new Set();
const observedContainers = new Set();
const containerProbe = profile === 'container'
  ? setInterval(() => {
      for (const id of managedContainerIds()) {
        if (!baselineContainers.has(id)) observedContainers.add(id);
      }
    }, 100)
  : undefined;

try {
  let transcript;
  try {
    // Receipt decision L1: C5 has an exact SDK receipt; E1 requires that exact
    // receipt processed with marker and thread running->idle effects. K1 no prior
    // transcript can satisfy the live gate. D1=C1-C5=>E1.
    const receipt = (await client.beta.sessions.events.send(session.id, {
      events: [{
        type: 'user.message',
        content: [{ type: 'text', text: `Reply with exactly this text and nothing else: ${marker}` }],
      }],
      betas: BETAS,
    })).data[0];
    ({ events: transcript } = await waitForSessionEventReceipt(
      client,
      session.id,
      receipt.id,
      BETAS,
      ({ delta }) => JSON.stringify(delta).includes(marker)
        && delta.some((event) => event.type === 'session.thread_status_running')
        && delta.some((event) => event.type === 'session.thread_status_idle'),
      'real Codex ACP marker and running-to-idle lifecycle',
      { timeoutMs: 600_000, pollMs: 200 },
    ));
  } finally {
    if (containerProbe !== undefined) clearInterval(containerProbe);
  }

  if (profile === 'container') {
    assert.ok(
      observedContainers.size > 0,
      'the Codex reply completed without observing a newly-created awaken.sandbox container',
    );
  }

  const replies = transcript
    .filter((event) => event.type === 'agent.message')
    .map((event) => (event.content ?? []).map((content) => content.text ?? '').join('').trim());

  assert.ok(
    replies.some((reply) => reply.includes(marker)),
    `real Codex ACP reply did not contain the marker; replies=${JSON.stringify(replies)}`,
  );
  assert.ok(
    transcript.some((event) => event.type === 'session.thread_status_running'),
    `the managed transcript did not expose a thread running state; events=${transcript.map((event) => event.type)}`,
  );
  assert.ok(
    transcript.some((event) => event.type === 'session.thread_status_idle'),
    `the managed transcript did not return the thread to idle; events=${transcript.map((event) => event.type)}`,
  );

  console.log(
    `CODEX ACP ${profile.toUpperCase()} E2E PASS: ${session.id} committed ${marker}`,
  );
  if (profile !== 'container') {
    await client.beta.sessions.delete(session.id, { betas: BETAS });
    await verifyAgentResources({
      client, runtime, temp: deployment.temp, restart: deployment.restart,
      expectedPhysicalRoot: deployment.physicalRoot,
      requireScopedExecution: scopedHost,
      ...(native ? { nativeModel: 'fake-haiku', nativeIsolation: nativeHost ? 'workdir' : 'namespace' } : {}),
      request: (method, route, body) =>
        request(deployment.baseURL, deployment.apiKey, method, route, body),
    });
    deployment.verifyCredentials();
    if (!native) console.log(`WORKER LOCAL CREDENTIAL CUSTODY PASS: ${runtime}, startup/restarts/resources`);
  }
} finally {
  await deployment.cleanup();
}
