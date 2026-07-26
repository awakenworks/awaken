// End-to-end coverage for the aggregated `awaken start` composition.
//
// The command adapter owns argument parsing and the build owns the one embedded
// console distribution; the server adapter owns static delivery while preserving
// the management API as the fallback. Drive both through the shipped binary so
// neither adapter is treated as an unobservable implementation detail.

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

  // Cause graph for command selection:
  // C1 command is `start`; C2 command is `serve`; C3 an unknown option is
  // present. E1 starts the interactive surface, E2 starts headless, E3 rejects
  // before binding a port or opening the data directory.
  // Decision table:
  // | Rule | C1 | C2 | C3 | Expected effect |
  // | K1   | T  | F  | F  | E1              |
  // | K2   | F  | T  | F  | E2              |
  // | K3   | -  | -  | T  | E3              |

  const help = spawnSync(bin, ['--help'], { encoding: 'utf8' });
  assert.equal(help.status, 0, help.stderr);
  assert.match(help.stdout, /awaken start/);
  assert.doesNotMatch(help.stdout, /AWAKEN_WEB_DIST/);

  const startHelp = spawnSync(bin, ['start', '--help'], { encoding: 'utf8' });
  assert.equal(startHelp.status, 0, startHelp.stderr);
  assert.match(startHelp.stdout, /USAGE/);

  const badArgs = spawnSync(bin, ['serve', '--unknown-option'], { encoding: 'utf8' });
  assert.notEqual(badArgs.status, 0);
  assert.match(badArgs.stderr, /unexpected argument/);

  const temp = fs.mkdtempSync(path.join(os.tmpdir(), 'awaken-console-e2e-'));
  const home = path.join(temp, 'home');
  const configDir = path.join(home, '.awaken');
  fs.mkdirSync(configDir, { recursive: true });
  fs.writeFileSync(path.join(configDir, 'config.toml'), [
    `data_dir = ${JSON.stringify(path.join(temp, 'data'))}`,
    `bind = ${JSON.stringify(`127.0.0.1:${PORT}`)}`,
  ].join('\n'));
  let server = spawn(bin, ['start', '--no-browser'], {
    cwd: temp,
    env: {
      ...process.env,
      HOME: home,
      // A legacy override must not revive the removed runtime-discovery path.
      AWAKEN_WEB_DIST: path.join(temp, 'does-not-exist'),
    },
    stdio: ['ignore', 'inherit', 'inherit'],
  });
  const stop = async () => {
    if (server.exitCode !== null) return;
    // Shutdown decision table:
    // | SIGINT exits within grace | process still live | action |
    // | true                     | false              | cleanup |
    // | false                    | true               | SIGKILL, await exit, cleanup |
    // Waiting for the terminal event after SIGKILL is required on Windows:
    // the child keeps its cwd/SQLite handles until process teardown completes.
    const exited = new Promise((resolve) => server.once('exit', resolve));
    server.kill('SIGINT');
    const graceful = await Promise.race([
      exited.then(() => true),
      sleep(10_000).then(() => false),
    ]);
    if (!graceful && server.exitCode === null && server.signalCode === null) {
      server.kill('SIGKILL');
      await exited;
    }
  };

  try {
    await waitForPort(PORT);
    const base = `http://127.0.0.1:${PORT}`;

    let response = await fetch(`${base}/`);
    assert.equal(response.status, 200);
    assert.match(await response.text(), /Awaken Console/);

    response = await fetch(`${base}/w/default/agents/new`);
    assert.equal(response.status, 200);
    assert.match(await response.text(), /Awaken Console/);

    response = await fetch(`${base}/assets/does-not-exist`);
    assert.equal(response.status, 404);

    response = await fetch(`${base}/v1/capabilities`);
    assert.equal(response.status, 200, await response.text());

    response = await fetch(`${base}/v1/does-not-exist`);
    assert.equal(response.status, 404);

    console.log(
      'CONSOLE START TS E2E PASS: command modes fail closed and the one embedded SPA source preserves the management API fallback.',
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
