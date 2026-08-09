// End-to-end coverage for the canonical Awaken process-role commands.
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
import { spawn, spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import { automatedAllInOneArgs } from './awaken_cli_args.mjs';
// @ts-expect-error The shared Cargo artifact resolver is intentionally JavaScript.
import { AWAKEN_BIN_ENV, cargoExecutable } from './cargo_binary.mjs';

const REPO_ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const PORT = Number(process.env.E2E_PORT ?? 39418);
const sleep = (ms: number) => new Promise((resolve) => setTimeout(resolve, ms));

function awakenBin() {
  return cargoExecutable({
    cwd: REPO_ROOT,
    packageName: 'awaken-cli',
    targetName: 'awaken',
    prebuiltEnvironmentName: AWAKEN_BIN_ENV,
  });
}

function waitForPort(port, server, timeoutMs = 180_000) {
  const deadline = performance.now() + timeoutMs;
  return new Promise((resolve, reject) => {
    const attempt = () => {
      const socket = net.createConnection({ port, host: '127.0.0.1' });
      socket.once('connect', () => {
        socket.destroy();
        resolve();
      });
      socket.once('error', () => {
        socket.destroy();
        if (server.exitCode !== null || server.signalCode !== null) {
          reject(new Error(`server exited before listening on ${port}`));
        } else if (performance.now() > deadline) reject(new Error(`server did not listen on ${port}`));
        else setTimeout(attempt, 100);
      });
    };
    attempt();
  });
}

async function main() {
  const bin = awakenBin();

  // Cause graph for command selection:
  // C1 command is absent/all-in-one; C2 a retired overlapping name is used;
  // C3 an unknown option is present; C4 Control uses local mode; C5 Control
  // uses server mode; C6 the split Control service token is projected; C7 the
  // executable-registration Coordinator URL/token pair is projected. E1
  // selects the one combined process, E2/E3/E4 reject before binding, and E5
  // exposes Control without Coordinator routes. Missing C6 masks the mode gate
  // because an unauthenticated split boundary must fail first.
  // Decision table:
  // | Rule | command/mode | retired | bad option | C6 | Expected effect |
  // | K1 | all-in-one | F | F | n/a | E1 |
  // | K2 | retired | T | F | n/a | E2 |
  // | K3 | all-in-one | F | T | n/a | E3 |
  // | K4a | Control/local | F | F | F | reject missing service token |
  // | K4b | Control/local | F | F | T; C7 absent | E4 (`mode=server` required) |
  // | K5 | Control/server | F | F | T; C7 paired | E5 |

  const help = spawnSync(bin, ['--help'], { encoding: 'utf8' });
  assert.equal(help.status, 0, help.stderr);
  assert.match(help.stdout, /all-in-one/);
  assert.match(help.stdout, /control/);
  assert.match(help.stdout, /coordinator/);
  assert.doesNotMatch(help.stdout, /AWAKEN_WEB_DIST/);

  const allInOneHelp = spawnSync(bin, automatedAllInOneArgs('--help'), { encoding: 'utf8' });
  assert.equal(allInOneHelp.status, 0, allInOneHelp.stderr);
  assert.match(allInOneHelp.stdout, /USAGE/);

  for (const retired of ['start', 'serve', 'management']) {
    const result = spawnSync(bin, [retired], { encoding: 'utf8' });
    assert.notEqual(result.status, 0, `retired command ${retired} unexpectedly succeeded`);
    assert.match(result.stderr, /unknown command/);
  }

  const badArgs = spawnSync(bin, automatedAllInOneArgs('--unknown-option'), { encoding: 'utf8' });
  assert.notEqual(badArgs.status, 0);
  assert.match(badArgs.stderr, /unexpected argument/);

  const temp = fs.mkdtempSync(path.join(os.tmpdir(), 'awaken-console-e2e-'));
  const home = path.join(temp, 'home');
  const configDir = path.join(home, '.awaken');
  fs.mkdirSync(configDir, { recursive: true });
  fs.writeFileSync(path.join(configDir, 'config.toml'), [
    `data_dir = ${JSON.stringify(path.join(temp, 'data'))}`,
    `bind = ${JSON.stringify(`127.0.0.1:${PORT}`)}`,
    'identity_mode = "no-login"',
    'acp_clis = ["gemini"]',
  ].join('\n'));
  const missingTokenControl = spawnSync(bin, ['control', '--config', path.join(configDir, 'config.toml')], {
    encoding: 'utf8',
    env: { ...process.env, HOME: home },
  });
  assert.notEqual(missingTokenControl.status, 0, 'K4a');
  assert.match(missingTokenControl.stderr, /requires control_service_token_file/, 'K4a');

  const serviceToken = path.join(temp, 'control-service-token');
  fs.writeFileSync(serviceToken, 'service-role-e2e-token\n', { mode: 0o600 });
  const localControlConfig = path.join(configDir, 'local-control.toml');
  fs.writeFileSync(localControlConfig, [
    `data_dir = ${JSON.stringify(path.join(temp, 'control-data'))}`,
    `bind = ${JSON.stringify(`127.0.0.1:${PORT}`)}`,
    'identity_mode = "no-login"',
    `control_service_token_file = ${JSON.stringify(serviceToken)}`,
  ].join('\n'));
  const localControl = spawnSync(bin, ['control', '--config', localControlConfig], {
    encoding: 'utf8',
    env: { ...process.env, HOME: home },
  });
  assert.notEqual(localControl.status, 0, 'K4b');
  assert.match(localControl.stderr, /requires mode = "server"/, 'K4b');

  let server = spawn(bin, automatedAllInOneArgs(), {
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
    await waitForPort(PORT, server);
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

    await stop();
    fs.writeFileSync(path.join(configDir, 'config.toml'), [
      `data_dir = ${JSON.stringify(path.join(temp, 'data'))}`,
      `bind = ${JSON.stringify(`127.0.0.1:${PORT}`)}`,
      'mode = "server"',
      'identity_mode = "no-login"',
      'control_seal_key = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff"',
      `control_service_token_file = ${JSON.stringify(serviceToken)}`,
      'coordinator_internal_url = "http://127.0.0.1:1"',
      `executable_agent_registration_token_file = ${JSON.stringify(serviceToken)}`,
    ].join('\n'));
    server = spawn(bin, ['control', '--config', path.join(configDir, 'config.toml')], {
      cwd: temp,
      env: { ...process.env, HOME: home },
      stdio: ['ignore', 'inherit', 'inherit'],
    });
    await waitForPort(PORT, server);
    response = await fetch(`${base}/v1/config/catalog`);
    assert.equal(response.status, 200, await response.text());
    response = await fetch(`${base}/v1/sessions`, {
      headers: { 'anthropic-beta': 'managed-agents-2026-04-01' },
    });
    assert.equal(response.status, 404, 'K5');

    console.log(
      'SERVICE ROLES E2E PASS: canonical commands fail closed and all-in-one preserves the embedded console plus combined API.',
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
