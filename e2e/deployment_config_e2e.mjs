// Typed deployment gate + split admin surface against the real `awaken` binary.
// Product environment variables are deliberately poisoned: only the explicit or
// standard config.toml and command presentation overrides may affect deployment.

import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { spawn } from 'node:child_process';
import { automatedAllInOneArgs } from './awaken_cli_args.mjs';
import { WORKER_BIN_ENV, cargoExecutable } from './cargo_binary.mjs';
import {
  REPO_ROOT,
  deploymentEnv,
  ensureProductionBuilt,
  initializeE2EInstallation,
  pass,
  stopServer,
  waitForPort,
} from './harness.mjs';

const BASE_PORT = Number(process.env.E2E_PORT ?? 38431);
const SEAL_KEY = '00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff';

function configPath(env) {
  return path.join(env.HOME, '.awaken', 'config.toml');
}

function runToExit(bin, args, env, timeoutMs = 20_000) {
  return new Promise((resolve) => {
    const child = spawn(bin, args, {
      env: { ...process.env, ...env },
      stdio: ['ignore', 'pipe', 'pipe'],
    });
    let output = '';
    child.stdout.on('data', (chunk) => (output += chunk.toString()));
    child.stderr.on('data', (chunk) => (output += chunk.toString()));
    const timer = setTimeout(() => child.kill('SIGKILL'), timeoutMs);
    child.once('exit', (code, signal) => {
      clearTimeout(timer);
      resolve({ code, signal, output });
    });
  });
}

async function rejected(bin, fields, expected) {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'awaken-config-reject-'));
  try {
    const env = deploymentEnv(root, { identityMode: 'no-login', fields });
    const result = await runToExit(bin, automatedAllInOneArgs('--config', configPath(env)), env);
    assert.notEqual(result.code, 0, `configuration unexpectedly booted: ${result.output}`);
    assert.match(result.output, expected);
  } finally {
    fs.rmSync(root, { recursive: true, force: true });
  }
}

async function main() {
  const bin = ensureProductionBuilt();

  // Process-boundary cause graph: C1 `awaken worker` attempts to revive the
  // removed embedded-worker command; C2 the standalone `awaken-worker` has a
  // typed Worker config but neither CLI nor file server authority; C3 it has an
  // exact server (covered by the remote-Worker suites). Effects: C1 rejects and
  // names the sole binary; C2 rejects before network I/O; C3 proceeds to normal
  // bootstrap. This prevents the test from preserving a duplicate CLI path.
  //
  // | Rule | Process | Server authority | Effect |
  // |---|---|---|---|
  // | W1 | aggregated `awaken` | any | unknown command; direct to `awaken-worker` |
  // | W2 | `awaken-worker` | absent | typed configuration rejection |
  // | W3 | `awaken-worker` | exact | bootstrap (owned by remote Worker E2E) |
  const removedWorker = await runToExit(bin, ['worker'], {});
  assert.notEqual(removedWorker.code, 0);
  assert.match(removedWorker.output, /separate `awaken-worker` binary/u);

  const workerBin = cargoExecutable({
    cwd: REPO_ROOT,
    packageName: 'awaken-worker',
    targetName: 'awaken-worker',
    prebuiltEnvironmentName: WORKER_BIN_ENV,
  });
  const workerRoot = fs.mkdtempSync(path.join(os.tmpdir(), 'awaken-worker-config-'));
  try {
    const workerConfig = path.join(workerRoot, 'config.toml');
    fs.writeFileSync(workerConfig, [
      'role = "worker"',
      'mode = "server"',
      `data_dir = ${JSON.stringify(workerRoot)}`,
    ].join('\n'));
    const worker = await runToExit(workerBin, ['--config', workerConfig], {});
    assert.notEqual(worker.code, 0);
    assert.match(worker.output, /Worker requires --server or worker_server/u);
  } finally {
    fs.rmSync(workerRoot, { recursive: true, force: true });
  }
  pass('standalone Worker is the sole execution-process entry and requires exact server authority');

  await rejected(bin, { run_local_pool: false }, /run_local_pool=false requires runtime_database_url/u);
  pass('pool-less coordinator requires a typed shared dispatch store');

  await rejected(bin, { resource_database_url: path.join(os.tmpdir(), 'resources.sqlite') }, /must be postgres:\/\//u);
  pass('resource plane rejects a second embedded database path');

  // Shared-topology cause/effect table: local dispatch + embedded Resources is
  // valid (covered by server startup below); shared dispatch + embedded
  // Resources is rejected before I/O; shared dispatch + shared Resources moves
  // past topology validation (covered by Postgres suites). `admin_db` remains a
  // Control-owned input and is never required by the Coordinator runtime.
  //
  // | Rule | Dispatch | Resources | Effect |
  // |---|---|---|---|
  // | S1 | local | embedded | accept local topology |
  // | S2 | shared | embedded | reject before connection |
  // | S3 | shared | shared | accept topology; connect normally |
  await rejected(
    bin,
    { runtime_database_url: 'postgres://127.0.0.1:1/never-connect' },
    /shared runtime requires resource_database_url to use Postgres/u,
  );
  pass('shared runtime rejects split local resource ownership before connecting');

  // Environment values name contradictory roles, roots, ports, stores, and
  // credentials. `config --json` must still report only typed file values.
  const isolationRoot = fs.mkdtempSync(path.join(os.tmpdir(), 'awaken-config-isolation-'));
  try {
    const env = deploymentEnv(isolationRoot, {
      identityMode: 'no-login',
      fields: { bind: `127.0.0.1:${BASE_PORT}`, role: 'all-in-one', run_local_pool: true },
    });
    Object.assign(env, {
      AWAKEN_ROLE: 'worker',
      AWAKEN_HTTP_ADDR: '127.0.0.1:1',
      AWAKEN_DEPLOYMENT_DATA_DIR: path.join(os.tmpdir(), 'forbidden-awaken-data'),
      AWAKEN_RUNTIME_DISPATCH_DATABASE_URL: 'postgres://forbidden',
      AWAKEN_CONTROL_SEAL_KEY: 'forbidden',
    });
    const result = await runToExit(bin, ['config', '--json', '--config', configPath(env)], env);
    assert.equal(result.code, 0, result.output);
    const report = JSON.parse(result.output);
    assert.equal(report.role, 'all-in-one');
    assert.equal(report.bind, `127.0.0.1:${BASE_PORT}`);
    assert.equal(report.data_dir, isolationRoot);
    assert.equal(report.runtime_dispatch_backend, 'sqlite');
    assert.ok(!result.output.includes('forbidden'));
    pass('deployment environment poisoning cannot alter typed configuration');
  } finally {
    fs.rmSync(isolationRoot, { recursive: true, force: true });
  }

  const serveRoot = fs.mkdtempSync(path.join(os.tmpdir(), 'awaken-admin-split-'));
  const httpPort = BASE_PORT + 1;
  const adminPort = BASE_PORT + 2;
  const env = deploymentEnv(serveRoot, {
    // Split-surface identity decision. C1 this scenario observes only the
    // deployment-owned readiness/metrics/drain listeners; C2 no IAM behavior
    // is under test. C1+C2 => select the canonical no-login mode so unrelated
    // Cloud OAuth cannot preempt topology startup. Constraint K: authentication
    // matrices retain sole ownership of identity behavior. Rule I1=C1+C2=>boot
    // the exact split surface; I2=identity scenarios=>use their own fixture.
    identityMode: 'no-login',
    controlSealKey: SEAL_KEY,
    fields: {
      bind: `127.0.0.1:${httpPort}`,
      admin_listen: `127.0.0.1:${adminPort}`,
      // Deployment topology is the subject of this scenario. Host ACP CLI
      // discovery has its own exact-install/restart/failure matrix and must not
      // make this startup depend on whichever tools happen to be installed.
      acp_clis: [],
    },
  });
  const serveEnvironment = { ...process.env, ...env, AWAKEN_HTTP_ADDR: '127.0.0.1:1' };
  initializeE2EInstallation(serveEnvironment, { binary: bin, configPath: configPath(env) });
  const serve = spawn(bin, automatedAllInOneArgs('--config', configPath(env)), {
    env: serveEnvironment,
    stdio: ['ignore', 'ignore', 'inherit'],
  });
  try {
    await waitForPort(adminPort, 180_000, serve);
    await waitForPort(httpPort, 180_000, serve);
    const admin = `http://127.0.0.1:${adminPort}`;
    assert.notEqual(await fetch(`http://127.0.0.1:${httpPort}/readyz`).then((r) => r.status), 200);
    assert.equal((await fetch(`${admin}/readyz`)).status, 200);
    assert.match(await fetch(`${admin}/metrics`).then((r) => r.text()), /awaken_brain_/u);
    assert.equal((await fetch(`${admin}/admin/drain`, { method: 'POST' })).status, 200);
    assert.equal((await fetch(`${admin}/readyz`)).status, 503);
    pass('typed admin_listen splits readiness, metrics, and drain from business traffic');
  } finally {
    await stopServer(serve);
    fs.rmSync(serveRoot, { recursive: true, force: true });
  }

  console.log('E2E PASS: typed deployment is fail-closed, environment-independent, and preserves the split admin surface.');
}

main().catch((error) => {
  console.error('E2E FAIL:', error);
  process.exitCode = 1;
});
