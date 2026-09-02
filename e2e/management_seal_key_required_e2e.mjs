// A server-mode control plane must have one explicit typed seal-key source.
// Local mode may create a durable owner-only key, but server mode cannot infer
// key custody or accept a retired environment-variable compatibility path.

import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { spawn } from 'node:child_process';
import { automatedAllInOneArgs } from './awaken_cli_args.mjs';
import {
  deploymentEnv,
  ensureProductionBuilt,
  initializeE2EInstallation,
  pass,
  stopServer,
  waitForPort,
} from './harness.mjs';

const PORT = Number(process.env.E2E_PORT ?? 38621);
const SEAL_KEY = '00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff';
const WORKSPACE = 'workspace-seal-key-required';

async function main() {
  const directory = fs.mkdtempSync(path.join(os.tmpdir(), 'awaken-sealkey-required-'));
  // Cause/effect table: C1=the exact installation is initialized with typed
  // key K; C2=the ordinary-start config then omits K; C3=the retired env carries
  // the same valid-looking K. E1=C1 removes installation continuity as a
  // competing failure; E2=C2+C3 rejects before bind for typed key custody. If
  // the retired input is revived, the initialized deployment can listen and
  // the existing oracle fails. Rule S1=C1+C2+C3=>E2.
  const initializedEnv = deploymentEnv(directory, {
    identityMode: 'no-login',
    controlSealKey: SEAL_KEY,
    fields: {
      mode: 'server',
      bind: `127.0.0.1:${PORT}`,
      expected_platform_workspace_id: WORKSPACE,
    },
  });
  initializeE2EInstallation({ ...process.env, ...initializedEnv });
  const env = deploymentEnv(directory, {
    identityMode: 'no-login',
    fields: {
      mode: 'server',
      bind: `127.0.0.1:${PORT}`,
      expected_platform_workspace_id: WORKSPACE,
    },
  });
  const server = spawn(ensureProductionBuilt(), automatedAllInOneArgs('--config', path.join(env.HOME, '.awaken', 'config.toml')), {
    env: {
      ...process.env,
      ...env,
      // This removed input is deliberately valid-looking. It must not satisfy
      // the typed server-mode key requirement.
      AWAKEN_MGMT_SEAL_KEY: SEAL_KEY,
    },
    stdio: ['ignore', 'ignore', 'pipe'],
  });
  let stderr = '';
  server.stderr.on('data', (chunk) => (stderr += chunk.toString()));
  try {
    const exited = new Promise((resolve) =>
      server.once('exit', (code, signal) => resolve({ code, signal })),
    );
    const listened = waitForPort(PORT, 3_000, server)
      .then(() => ({ listened: true }))
      .catch(() => ({ listened: false }));
    const outcome = await Promise.race([exited, listened]);
    assert.notEqual(outcome.listened, true, 'server mode must reject before bind');
    const terminal = 'code' in outcome ? outcome : await exited;
    assert.ok(terminal.code !== 0 || terminal.signal !== null, JSON.stringify(terminal));
    assert.match(stderr, /server mode requires control_seal_key or control_seal_key_file/u);
    pass('server mode rejects an absent typed seal-key source before bind');
    pass('the retired seal-key environment variable cannot satisfy key custody');
  } finally {
    await stopServer(server);
    fs.rmSync(directory, { recursive: true, force: true });
  }

  console.log('E2E PASS: server-mode seal-key custody is explicit, typed, and fail-closed.');
}

main().catch((error) => {
  console.error('E2E FAIL:', error);
  process.exitCode = 1;
});
