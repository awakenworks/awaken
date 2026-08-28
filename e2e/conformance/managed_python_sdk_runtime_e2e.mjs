import { spawn, spawnSync } from 'node:child_process';
import { mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { resolve } from 'node:path';

import {
  availablePort,
  deploymentEnv,
  realServerEnv,
  spawnServer,
  startUpstream,
  stopServer,
  waitForPort,
  withScenarioServer,
  withServer,
} from '../harness.mjs';

const REPO = resolve(import.meta.dirname, '../..');
const LOCK = resolve(REPO, 'packages/managed-sdk-oracle/python/requirements.lock');
const MATRIX_LOCK = resolve(
  REPO,
  'packages/managed-sdk-oracle/python/runtime-matrix-requirements.lock',
);
const ORACLE = resolve(REPO, 'contracts/anthropic-managed/python-upstream-oracle.generated.json');
const DRIVER = resolve(import.meta.dirname, 'managed_python_sdk_runtime_e2e.py');
const HELPER_DRIVER = resolve(import.meta.dirname, 'managed_python_sdk_helpers_e2e.py');
const MATRIX_DRIVER = resolve(import.meta.dirname, 'managed_python_sdk_matrix_e2e.py');
const PORT = Number(process.env.E2E_PORT ?? 38199);

function commandSucceeds(command, args) {
  const result = spawnSync(command, args, { stdio: 'ignore' });
  return !result.error && result.status === 0;
}

function provisionPython(directory) {
  const explicit = process.env.AWAKEN_PYTHON_SDK_PYTHON;
  const candidates = explicit
    ? [explicit]
    : ['python3', 'python3.14', 'python3.13', 'python3.12', 'python3.11', 'python3.10'];
  for (const candidate of candidates) {
    if (!commandSucceeds(candidate, ['-c', 'import sys; raise SystemExit(sys.version_info < (3, 10))'])) {
      continue;
    }
    const environment = resolve(directory, 'venv');
    rmSync(environment, { recursive: true, force: true });
    if (!commandSucceeds(candidate, ['-m', 'venv', environment])) continue;
    const python = resolve(environment, process.platform === 'win32' ? 'Scripts/python.exe' : 'bin/python');
    const pip = resolve(environment, process.platform === 'win32' ? 'Scripts/pip.exe' : 'bin/pip');
    const install = spawnSync(pip, [
      'install',
      '--disable-pip-version-check',
      '--no-input',
      '--requirement',
      LOCK,
    ], { stdio: 'inherit' });
    if (install.status !== 0) throw new Error(`failed to install Python SDK closure with ${candidate}`);
    return { python, pip };
  }
  throw new Error('Python Managed SDK E2E requires Python >=3.10 with the venv module');
}

function provisionHistoricalSdks(pip, directory, anchors) {
  const closure = spawnSync(pip, [
    'install',
    '--disable-pip-version-check',
    '--no-input',
    '--requirement',
    MATRIX_LOCK,
  ], { encoding: 'utf8' });
  if (closure.status !== 0) {
    throw new Error(`failed to install Python history closure:\n${closure.stdout}\n${closure.stderr}`);
  }
  const roots = new Map();
  for (const anchor of anchors) {
    const target = resolve(directory, 'versions', anchor.version);
    const requirement = resolve(directory, `anthropic-${anchor.version}.txt`);
    mkdirSync(target, { recursive: true });
    writeFileSync(
      requirement,
      `anthropic==${anchor.version} --hash=sha256:${anchor.wheel.sha256}\n`,
    );
    const installed = spawnSync(pip, [
      'install',
      '--disable-pip-version-check',
      '--no-input',
      '--only-binary=:all:',
      '--no-deps',
      '--require-hashes',
      '--target',
      target,
      '--requirement',
      requirement,
    ], { encoding: 'utf8' });
    if (installed.status !== 0) {
      throw new Error(
        `failed to install anthropic ${anchor.version}:\n${installed.stdout}\n${installed.stderr}`,
      );
    }
    roots.set(anchor.version, target);
  }
  return roots;
}

function runDriver(python, driver, baseURL, { args = [], extraEnv = {} } = {}) {
  return new Promise((resolveDriver, rejectDriver) => {
    const child = spawn(python, [driver, ...args], {
      env: {
        ...process.env,
        AWAKEN_MANAGED_BASE_URL: baseURL,
        AWAKEN_PYTHON_REQUIREMENTS_LOCK: LOCK,
        AWAKEN_PYTHON_ORACLE: ORACLE,
        ...extraEnv,
      },
      stdio: 'inherit',
    });
    child.once('error', rejectDriver);
    child.once('close', (status, signal) => {
      if (signal) rejectDriver(new Error(`Python Managed SDK driver terminated by ${signal}`));
      else if (status !== 0) rejectDriver(new Error(`Python Managed SDK driver exited ${status}`));
      else resolveDriver();
    });
  });
}

async function exerciseRecovery(python, temporary) {
  // Cross-process cause/effect graph: Node owns only topology and shutdown;
  // the exact Python wheel owns all API encoding/decoding in both phases. One
  // upstream survives A->B while the same durable directory is reopened.
  // Effects: the state JSON carries ids only, graceful shutdown flushes facts,
  // and a fresh client/process must prove Session/Event/Memory/File/Skill
  // recovery. Any child, bind, shutdown, or verification failure rejects.
  const storage = resolve(temporary, 'recovery-storage');
  const state = resolve(temporary, 'recovery-state.json');
  mkdirSync(storage, { recursive: true });
  const port = await availablePort(PORT + 2);
  const upstream = await startUpstream('echo');
  const environment = {
    ...deploymentEnv(storage, { identityMode: 'no-login' }),
    ...realServerEnv('echo', upstream, { mode: 'management' }),
  };
  let running;
  try {
    const first = spawnServer('management', port, environment);
    running = first.server;
    await waitForPort(port, 900_000, running);
    await runDriver(python, DRIVER, first.baseUrl, { args: ['prepare-recovery', state] });
    await stopServer(running);
    running = undefined;

    const second = spawnServer('management', port, environment);
    running = second.server;
    await waitForPort(port, 900_000, running);
    await runDriver(python, DRIVER, second.baseUrl, { args: ['verify-recovery', state] });
  } finally {
    if (running) await stopServer(running);
    upstream.close();
  }
}

async function exerciseHistoricalMatrix(python, pip, temporary) {
  // Version-axis graph: all configured historical rows are reviewed change
  // points, never arbitrary patches. Each target installation is constrained
  // by the generated official wheel SHA; one shared exact dependency closure
  // avoids ambient packages. The Python driver then re-extracts source evidence
  // and owns every SDK call. One live server is shared because service behavior
  // is invariant; wheel state is isolated by one subprocess/PYTHONPATH per row.
  const oracle = JSON.parse(readFileSync(ORACLE, 'utf8'));
  const anchors = oracle.anchors.filter(({ role }) => role !== 'current_oracle');
  const roots = provisionHistoricalSdks(pip, temporary, anchors);
  await withScenarioServer('management', 'echo', PORT + 3, async (baseURL) => {
    for (const anchor of anchors) {
      await runDriver(python, MATRIX_DRIVER, baseURL, {
        args: [anchor.version, baseURL],
        extraEnv: { PYTHONPATH: roots.get(anchor.version) },
      });
    }
  });
}

// Multi-language runtime cause/effect graph:
// C1 the reviewed lock provisions one isolated official Python client closure;
// C2 the real Awaken process speaks to a real fake-provider socket; C3 the
// core driver crosses sync/async, beta/GA, paging, SSE, errors and multipart
// boundaries; C4 the helper driver crosses the official poller, SessionToolRunner,
// EnvironmentWorker and local Agent Toolset against their owning topology.
// Effects: E1 no globally installed package can impersonate the oracle; E2
// client encoding/decoding and handwritten helper composition are end-to-end;
// E3 install, child, signal, or protocol failure fails the release gate.
// Decision table: C1+C2+C3+C4=>E1+E2; missing venv/install/nonzero/signal=>E3.
const temporary = mkdtempSync(resolve(tmpdir(), 'awaken-python-managed-sdk-'));
try {
  const { python, pip } = provisionPython(temporary);
  await withScenarioServer('management', 'echo', PORT, async (baseURL) => {
    await runDriver(python, DRIVER, baseURL);
  });
  await withServer('worker', PORT + 1, async (baseURL) => {
    await runDriver(python, HELPER_DRIVER, baseURL);
  });
  await exerciseRecovery(python, temporary);
  await exerciseHistoricalMatrix(python, pip, temporary);
  const pinned = readFileSync(LOCK, 'utf8').match(/^anthropic==(\S+)$/mu)?.[1];
  console.log(`E2E PASS: official Python Managed SDK ${pinned} runtime compatibility.`);
} finally {
  rmSync(temporary, { recursive: true, force: true });
}
