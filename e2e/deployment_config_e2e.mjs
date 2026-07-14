// Deployment config gate (P4) + serve admin-port split (P2), end-to-end against the
// REAL aggregated `awaken` binary (not the scenario-host stub): the composition
// root's boot-time config validation and the split admin surface.
//
// Covers awaken-cli `config.rs` + `main.rs` (the config gate) and `brain_admin.rs`
// (the split admin router) through a real process, complementing the Rust unit tests.
//
// Run: node e2e/deployment_config_e2e.mjs

import assert from 'node:assert/strict';
import net from 'node:net';
import { spawn, execSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import path from 'node:path';
import { pass } from './harness.mjs';

const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');

// Resolve (building if needed) the aggregated `awaken` binary, honoring
// CARGO_TARGET_DIR so this attributes coverage under coverage.sh's instrumented build.
function awakenBin() {
  const out = execSync('cargo build --quiet --message-format=json -p awaken-cli --bin awaken', {
    cwd: ROOT,
    maxBuffer: 64 * 1024 * 1024,
  }).toString();
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

function freePort() {
  return new Promise((resolve, reject) => {
    const srv = net.createServer();
    srv.listen(0, '127.0.0.1', () => {
      const { port } = srv.address();
      srv.close(() => resolve(port));
    });
    srv.on('error', reject);
  });
}

function waitForPort(port, timeoutMs = 30_000) {
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
        if (Date.now() > deadline) reject(new Error(`nothing listening on ${port}`));
        else setTimeout(attempt, 150);
      });
    };
    attempt();
  });
}

// Run the binary to completion (expecting it to exit on its own), capturing stderr.
function runToExit(bin, env, timeoutMs = 20_000) {
  return new Promise((resolve) => {
    const child = spawn(bin, { env: { ...process.env, ...env }, stdio: ['ignore', 'pipe', 'pipe'] });
    let stderr = '';
    child.stdout.on('data', (c) => (stderr += c.toString()));
    child.stderr.on('data', (c) => (stderr += c.toString()));
    const timer = setTimeout(() => child.kill('SIGKILL'), timeoutMs);
    child.on('exit', (code) => {
      clearTimeout(timer);
      resolve({ code, stderr });
    });
  });
}

async function main() {
  const bin = awakenBin();

  // ── 1. The config gate REFUSES a contradictory deployment ────────────────
  // A worker role with no server URL cannot drain anything → fail closed at boot.
  const worker = await runToExit(bin, {
    AWAKEN_ROLE: 'worker',
    AWAKEN_WORKER_SERVE_URL: '',
    AWAKEN_UPSTREAM_URL: '',
  });
  assert.notEqual(worker.code, 0, `a server-URL-less worker must refuse to boot (got ${worker.code})`);
  assert.ok(
    worker.stderr.includes('AWAKEN_WORKER_SERVE_URL'),
    `the refusal names the missing key: ${worker.stderr}`,
  );
  pass('the config gate refuses a worker with no server URL');

  // A coordinator (no local pool) with no shared queue is also refused.
  const coord = await runToExit(bin, {
    AWAKEN_ROLE: 'serve',
    AWAKEN_SERVER_RUN_LOCAL_POOL: 'false',
    AWAKEN_RUNTIME_DISPATCH_DATABASE_URL: '',
    AWAKEN_DATABASE_URL: '',
  });
  assert.notEqual(coord.code, 0, 'a pool-less coordinator with no shared queue must refuse to boot');
  assert.ok(
    coord.stderr.includes('shared Postgres dispatch queue'),
    `the refusal explains the missing queue: ${coord.stderr}`,
  );
  pass('the config gate refuses a coordinator with no shared queue');

  // ── 2. Legacy env names still read, with a deprecation warning ───────────
  // A worker with the LEGACY upstream name boots past the gate (valid config) but
  // warns; we only need to see the warning, so point it at an unused URL and kill it.
  {
    const httpPort = await freePort();
    const child = spawn(bin, {
      env: {
        ...process.env,
        AWAKEN_ROLE: 'worker',
        AWAKEN_UPSTREAM_URL: `http://127.0.0.1:${httpPort}`,
        AWAKEN_INGRESS: 'durable',
      },
      stdio: ['ignore', 'pipe', 'pipe'],
    });
    let out = '';
    child.stdout.on('data', (c) => (out += c.toString()));
    child.stderr.on('data', (c) => (out += c.toString()));
    // Give it a moment to print the deprecation + config summary, then stop it.
    await new Promise((r) => setTimeout(r, 2500));
    child.kill('SIGINT');
    await new Promise((r) => child.on('exit', r));
    assert.ok(
      out.includes('deprecated') && out.includes('AWAKEN_UPSTREAM_URL'),
      `a legacy env name warns: ${out}`,
    );
    pass('a legacy env name (AWAKEN_UPSTREAM_URL) still reads and warns');
  }

  // ── 3. Serve admin-port split (P2): probes/drain on a SEPARATE port ───────
  const httpPort = await freePort();
  const adminPort = await freePort();
  const serve = spawn(bin, {
    env: {
      ...process.env,
      AWAKEN_HTTP_ADDR: `127.0.0.1:${httpPort}`,
      AWAKEN_SERVER_ADMIN_LISTEN: `127.0.0.1:${adminPort}`,
      AWAKEN_MODEL_MODE: 'echo',
    },
    stdio: ['ignore', 'inherit', 'inherit'],
  });
  try {
    await waitForPort(adminPort);
    const admin = `http://127.0.0.1:${adminPort}`;
    // The business port must NOT serve the admin routes (they live on the admin
    // port): a probe there is anything but a 200 "ready" (404 unrouted, or an auth
    // rejection — either way not the admin surface).
    await waitForPort(httpPort);
    const bizReadyz = await fetch(`http://127.0.0.1:${httpPort}/readyz`).then((r) => r.status);
    assert.notEqual(bizReadyz, 200, 'the business port does not serve admin /readyz (it is split off)');

    // The admin port serves readiness, metrics, and drain.
    const ready = await fetch(`${admin}/readyz`);
    assert.equal(ready.status, 200, 'admin /readyz is 200 before draining');

    const metrics = await fetch(`${admin}/metrics`).then((r) => r.text());
    assert.ok(metrics.includes('awaken_brain_'), `admin /metrics exposes the brain gauges: ${metrics}`);

    const drained = await fetch(`${admin}/admin/drain`, { method: 'POST' });
    assert.equal(drained.status, 200, 'POST /admin/drain succeeds');

    const afterDrain = await fetch(`${admin}/readyz`);
    assert.equal(afterDrain.status, 503, 'admin /readyz flips to 503 after drain');
    pass('the serve admin surface splits onto its own port (readyz/metrics/drain)');
  } finally {
    serve.kill('SIGINT');
    await new Promise((r) => serve.on('exit', r));
  }

  console.log('E2E PASS: deployment config gate refuses bad shapes, warns on legacy names, and the admin surface splits onto its own port.');
}

main().catch((err) => {
  console.error('E2E FAIL:', err);
  process.exitCode = 1;
});
