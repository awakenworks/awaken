// End-to-end coverage for the aggregated `awaken start` composition.
//
// The command adapter owns argument parsing and console distribution discovery;
// the server adapter owns static delivery while preserving the management API as
// the fallback. Drive both through the shipped binary so neither adapter is
// treated as an unobservable implementation detail.

import assert from 'node:assert/strict';
import fs from 'node:fs';
import net from 'node:net';
import os from 'node:os';
import path from 'node:path';
import { execFileSync, execSync, spawn, spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';

const REPO_ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const PORT = Number(process.env.E2E_PORT ?? 39412);
const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

function awakenBin() {
  const output = execSync(
    'cargo build --quiet --message-format=json -p awaken-cli --bin awaken',
    { cwd: REPO_ROOT, maxBuffer: 64 * 1024 * 1024 },
  ).toString();
  for (const line of output.split('\n')) {
    if (!line.trim()) continue;
    try {
      const message = JSON.parse(line);
      if (message.executable && message.target?.name === 'awaken') return message.executable;
    } catch {
      // Cargo may interleave non-JSON diagnostics.
    }
  }
  throw new Error('could not resolve the awaken binary path');
}

function waitForPort(port, timeoutMs = 60_000) {
  const deadline = Date.now() + timeoutMs;
  return new Promise((resolve, reject) => {
    const attempt = () => {
      const socket = net.createConnection({ port, host: '127.0.0.1' });
      socket.once('connect', () => {
        socket.destroy();
        resolve();
      });
      socket.once('error', () => {
        socket.destroy();
        if (Date.now() > deadline) reject(new Error(`server did not listen on ${port}`));
        else setTimeout(attempt, 100);
      });
    };
    attempt();
  });
}

async function main() {
  const bin = awakenBin();

  const help = spawnSync(bin, ['--help'], { encoding: 'utf8' });
  assert.equal(help.status, 0, help.stderr);
  assert.match(help.stdout, /awaken start/);
  assert.match(help.stdout, /AWAKEN_WEB_DIST/);

  const startHelp = spawnSync(bin, ['start', '--help'], { encoding: 'utf8' });
  assert.equal(startHelp.status, 0, startHelp.stderr);
  assert.match(startHelp.stdout, /USAGE/);

  const badArgs = spawnSync(bin, ['serve'], { encoding: 'utf8' });
  assert.notEqual(badArgs.status, 0);
  assert.match(badArgs.stderr, /unknown arguments/);

  const temp = fs.mkdtempSync(path.join(os.tmpdir(), 'awaken-console-e2e-'));
  const invalidDist = path.join(temp, 'invalid-dist');
  fs.mkdirSync(invalidDist);
  const invalid = spawnSync(bin, ['start'], {
    encoding: 'utf8',
    env: { ...process.env, AWAKEN_WEB_DIST: invalidDist },
  });
  assert.notEqual(invalid.status, 0);
  assert.match(invalid.stderr, /does not contain index\.html/);

  const dist = path.join(temp, 'dist');
  fs.mkdirSync(path.join(dist, 'assets'), { recursive: true });
  fs.writeFileSync(path.join(dist, 'index.html'), '<!doctype html><title>Awaken Console E2E</title>');
  fs.writeFileSync(path.join(dist, 'assets', 'probe.txt'), 'console-asset');

  const server = spawn(bin, ['start'], {
    env: {
      ...process.env,
      AWAKEN_HTTP_ADDR: `127.0.0.1:${PORT}`,
      AWAKEN_LOCAL_WORKSPACE_ID: `workspace_console_${process.pid}`,
      AWAKEN_WEB_DIST: dist,
    },
    stdio: ['ignore', 'inherit', 'inherit'],
  });
  const stop = async () => {
    if (server.exitCode !== null) return;
    server.kill('SIGINT');
    await Promise.race([
      new Promise((resolve) => server.once('exit', resolve)),
      sleep(10_000).then(() => {
        server.kill('SIGKILL');
      }),
    ]);
  };

  try {
    await waitForPort(PORT);
    const base = `http://127.0.0.1:${PORT}`;

    let response = await fetch(`${base}/`);
    assert.equal(response.status, 200);
    assert.match(await response.text(), /Awaken Console E2E/);

    response = await fetch(`${base}/w/default/agents/new`);
    assert.equal(response.status, 200);
    assert.match(await response.text(), /Awaken Console E2E/);

    response = await fetch(`${base}/assets/probe.txt`);
    assert.equal(response.status, 200);
    assert.equal(await response.text(), 'console-asset');

    response = await fetch(`${base}/v1/capabilities`);
    assert.equal(response.status, 200, await response.text());

    response = await fetch(`${base}/v1/does-not-exist`);
    assert.equal(response.status, 404);

    console.log(
      'CONSOLE START TS E2E PASS: command modes fail closed and one process serves the SPA plus the unchanged management API fallback.',
    );
  } finally {
    await stop();
    fs.rmSync(temp, { recursive: true, force: true });
  }
}

main().catch((error) => {
  console.error(error);
  process.exitCode = 1;
});
