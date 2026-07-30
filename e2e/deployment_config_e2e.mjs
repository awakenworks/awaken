// Typed deployment gate + split admin surface against the real `awaken` binary.
// Product environment variables are deliberately poisoned: only the explicit or
// standard config.toml and command presentation overrides may affect deployment.

import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { spawn } from 'node:child_process';
import {
  deploymentEnv,
  ensureProductionBuilt,
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
    const env = deploymentEnv(root, { fields });
    const result = await runToExit(bin, ['all-in-one', '--config', configPath(env)], env);
    assert.notEqual(result.code, 0, `configuration unexpectedly booted: ${result.output}`);
    assert.match(result.output, expected);
  } finally {
    fs.rmSync(root, { recursive: true, force: true });
  }
}

async function main() {
  const bin = ensureProductionBuilt();

  // Cause graph: a Worker without a server cannot drain work; a coordinator
  // without a local pool needs one shared queue; shared runtime ownership needs
  // one shared resource/catalog plane. Every contradiction fails before I/O.
  const missingWorkerConfig = path.join(os.tmpdir(), 'not-read-without-server.toml');
  const worker = await runToExit(bin, ['worker', '--config', missingWorkerConfig], {});
  assert.notEqual(worker.code, 0);
  assert.match(worker.output, /worker requires --server/u);
  pass('worker role requires an exact typed worker_server');

  await rejected(bin, { run_local_pool: false }, /run_local_pool=false requires runtime_database_url/u);
  pass('pool-less coordinator requires a typed shared dispatch store');

  await rejected(bin, { resource_database_url: path.join(os.tmpdir(), 'resources.sqlite') }, /must be postgres:\/\//u);
  pass('resource plane rejects a second embedded database path');

  await rejected(
    bin,
    { runtime_database_url: 'postgres://127.0.0.1:1/never-connect' },
    /requires resource_database_url and admin_db/u,
  );
  pass('shared runtime rejects split local resource ownership before connecting');

  // Environment values name contradictory roles, roots, ports, stores, and
  // credentials. `config --json` must still report only typed file values.
  const isolationRoot = fs.mkdtempSync(path.join(os.tmpdir(), 'awaken-config-isolation-'));
  try {
    const env = deploymentEnv(isolationRoot, {
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
    controlSealKey: SEAL_KEY,
    fields: {
      bind: `127.0.0.1:${httpPort}`,
      admin_listen: `127.0.0.1:${adminPort}`,
    },
  });
  const serve = spawn(bin, ['all-in-one', '--config', configPath(env)], {
    env: { ...process.env, ...env, AWAKEN_HTTP_ADDR: '127.0.0.1:1' },
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
