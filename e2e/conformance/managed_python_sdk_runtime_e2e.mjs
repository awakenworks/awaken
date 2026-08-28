import { spawn, spawnSync } from 'node:child_process';
import { mkdtempSync, readFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { resolve } from 'node:path';

import { withScenarioServer } from '../harness.mjs';

const REPO = resolve(import.meta.dirname, '../..');
const LOCK = resolve(REPO, 'packages/managed-sdk-oracle/python/requirements.lock');
const DRIVER = resolve(import.meta.dirname, 'managed_python_sdk_runtime_e2e.py');
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
    return python;
  }
  throw new Error('Python Managed SDK E2E requires Python >=3.10 with the venv module');
}

function runDriver(python, baseURL) {
  return new Promise((resolveDriver, rejectDriver) => {
    const child = spawn(python, [DRIVER], {
      env: {
        ...process.env,
        AWAKEN_MANAGED_BASE_URL: baseURL,
        AWAKEN_PYTHON_REQUIREMENTS_LOCK: LOCK,
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

// Multi-language runtime cause/effect graph:
// C1 the reviewed lock provisions one isolated official Python client closure;
// C2 the real Awaken process speaks to a real fake-provider socket; C3 the
// Python driver crosses sync/async, beta/GA, paging, SSE, errors and multipart
// boundaries. Effects: E1 no globally installed package can impersonate the
// oracle; E2 client encoding and decoding are exercised end to end; E3 install,
// child, signal, or protocol failure fails the release gate. Decision table:
// C1+C2+C3=>E1+E2; missing venv/install/nonzero/signal=>E3. The service-domain
// semantics remain owned by the shared TypeScript behavior owners; this driver
// owns only Python-specific transport and decoder behavior.
const temporary = mkdtempSync(resolve(tmpdir(), 'awaken-python-managed-sdk-'));
try {
  const python = provisionPython(temporary);
  await withScenarioServer('management', 'echo', PORT, async (baseURL) => {
    await runDriver(python, baseURL);
  });
  const pinned = readFileSync(LOCK, 'utf8').match(/^anthropic==(\S+)$/mu)?.[1];
  console.log(`E2E PASS: official Python Managed SDK ${pinned} runtime compatibility.`);
} finally {
  rmSync(temporary, { recursive: true, force: true });
}
