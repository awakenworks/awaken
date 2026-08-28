import assert from 'node:assert/strict';
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
import { extractOperations } from '../../packages/managed-sdk-oracle/src/extract-operations.mjs';
import {
  extractRequestContractsFromPackageRoot,
  extractResponseContractsFromPackageRoot,
} from '../../packages/managed-sdk-oracle/src/extract-wire-contracts.mjs';
import { resolveSdkPackage } from '../../packages/managed-sdk-oracle/src/package-source.mjs';
import {
  canonicalPythonOperationID,
} from '../../packages/managed-sdk-oracle/src/python-operation-identity.mjs';
import { buildRequestWitnessBundle } from './managed_sdk_request_contract_e2e.mjs';

const REPO = resolve(import.meta.dirname, '../..');
const LOCK = resolve(REPO, 'packages/managed-sdk-oracle/python/requirements.lock');
const MATRIX_LOCK = resolve(
  REPO,
  'packages/managed-sdk-oracle/python/runtime-matrix-requirements.lock',
);
const ORACLE = resolve(REPO, 'contracts/anthropic-managed/python-upstream-oracle.generated.json');
const SCOPE = resolve(REPO, 'packages/managed-sdk-oracle/config/scope.json');
const DRIVER = resolve(import.meta.dirname, 'managed_python_sdk_runtime_e2e.py');
const HELPER_DRIVER = resolve(import.meta.dirname, 'managed_python_sdk_helpers_e2e.py');
const MATRIX_DRIVER = resolve(import.meta.dirname, 'managed_python_sdk_matrix_e2e.py');
const RESPONSE_CONTRACT_DRIVER = resolve(
  import.meta.dirname,
  'managed_python_sdk_response_contract_e2e.py',
);
const REQUEST_CONTRACT_DRIVER = resolve(
  import.meta.dirname,
  'managed_python_sdk_request_contract_e2e.py',
);
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

function writePythonResponseContracts(temporary) {
  // One authority projection: Python ids differ only by generator spelling;
  // response structure comes exclusively from the adjacent declaration of the
  // reviewed TypeScript 0.122 candidate, whose transport change-point delta is
  // already proven identical to Python 1.2. No handwritten response corpus can
  // drift.
  const module = '@anthropic-ai/sdk-candidate';
  const sdk = resolveSdkPackage(module);
  const scope = JSON.parse(readFileSync(SCOPE, 'utf8'));
  const typescriptOperations = extractOperations(module, scope).operations;
  const typescriptContracts = extractResponseContractsFromPackageRoot(
    sdk.root,
    scope,
    typescriptOperations.map(({ id }) => id),
  );
  const python = JSON.parse(readFileSync(ORACLE, 'utf8')).current;
  const typescriptIDs = new Set(typescriptOperations.map(({ id }) => id));
  const canonical = python.operations.map(({ id }) => canonicalPythonOperationID(id));
  assert.equal(new Set(canonical).size, canonical.length, 'Python operation mapping is injective');
  assert.deepEqual(new Set(canonical), typescriptIDs, 'Python and candidate operation sets');

  const contracts = Object.fromEntries(python.operations.map(({ id }) => {
    const typescriptID = canonicalPythonOperationID(id);
    return [id, typescriptContracts[typescriptID]];
  }));
  const destination = resolve(temporary, 'python-response-contracts.json');
  writeFileSync(destination, `${JSON.stringify({
    python_version: python.version,
    typescript_version: sdk.version,
    operations: python.operations,
    contracts,
  })}\n`);
  return destination;
}

async function writePythonRequestContracts(temporary) {
  // One declaration and one executable authority: the reviewed TS 0.122
  // candidate supplies both its finite request graph and generated serializer.
  // Python operation ids are an injective spelling projection only; every
  // witness retains the TS-emitted semantic Request as its expected result.
  const module = '@anthropic-ai/sdk-candidate';
  const sdk = resolveSdkPackage(module);
  const scope = JSON.parse(readFileSync(SCOPE, 'utf8'));
  const operations = extractOperations(module, scope).operations;
  const contracts = extractRequestContractsFromPackageRoot(
    sdk.root,
    scope,
    operations.map(({ id }) => id),
  );
  const python = JSON.parse(readFileSync(ORACLE, 'utf8')).current;
  const pythonByTypescript = new Map(
    python.operations.map(({ id }) => [canonicalPythonOperationID(id), id]),
  );
  assert.equal(pythonByTypescript.size, python.operations.length, 'Python request mapping is injective');
  assert.deepEqual(
    new Set(pythonByTypescript.keys()),
    new Set(operations.map(({ id }) => id)),
    'Python and candidate request operation sets',
  );
  const bundle = await buildRequestWitnessBundle({ packageRoot: sdk.root, operations, contracts });
  const witnesses = bundle.witnesses.map((witness) => ({
    ...witness,
    python_operation_id: pythonByTypescript.get(witness.operation_id),
  }));
  const upstreamRejections = bundle.upstream_rejections.map((rejection) => ({
    ...rejection,
    python_operation_id: pythonByTypescript.get(rejection.operation_id),
  }));
  assert.ok(witnesses.every(({ python_operation_id }) => python_operation_id));
  assert.ok(upstreamRejections.every(({ python_operation_id }) => python_operation_id));
  const destination = resolve(temporary, 'python-request-contracts.json');
  writeFileSync(destination, `${JSON.stringify({
    operation_count: operations.length,
    python_version: python.version,
    typescript_version: sdk.version,
    upstream_rejections: upstreamRejections,
    witness_count: witnesses.length,
    witnesses,
  })}\n`);
  return destination;
}

async function exercisePythonResponseContracts(python, contracts) {
  await runDriver(python, RESPONSE_CONTRACT_DRIVER, 'http://managed-response.invalid', {
    extraEnv: { AWAKEN_MANAGED_PYTHON_RESPONSE_CONTRACTS: contracts },
  });
}

async function exercisePythonRequestContracts(python, contracts) {
  await runDriver(python, REQUEST_CONTRACT_DRIVER, 'http://managed-request.invalid', {
    extraEnv: { AWAKEN_MANAGED_PYTHON_REQUEST_CONTRACTS: contracts },
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

async function exerciseHistoricalMatrix(
  python,
  pip,
  temporary,
  responseContracts,
  selectedVersion,
) {
  // Version-axis graph: all configured historical rows are reviewed change
  // points, never arbitrary patches. Each target installation is constrained
  // by the generated official wheel SHA; one shared exact dependency closure
  // avoids ambient packages. The Python driver then re-extracts source evidence
  // and owns every SDK call. One live server is shared because service behavior
  // is invariant; wheel state is isolated by one subprocess/PYTHONPATH per row.
  const oracle = JSON.parse(readFileSync(ORACLE, 'utf8'));
  const historical = oracle.anchors.filter(({ role }) => role !== 'current_oracle');
  const anchors = selectedVersion
    ? historical.filter(({ version }) => version === selectedVersion)
    : historical;
  if (anchors.length === 0) {
    throw new Error(`unknown historical Python Managed SDK version ${JSON.stringify(selectedVersion)}`);
  }
  const roots = provisionHistoricalSdks(pip, temporary, anchors);
  await withScenarioServer('management', 'echo', PORT + 3, async (baseURL) => {
    for (const anchor of anchors) {
      await runDriver(python, MATRIX_DRIVER, baseURL, {
        args: [anchor.version, baseURL],
        extraEnv: {
          AWAKEN_MANAGED_PYTHON_RESPONSE_CONTRACTS: responseContracts,
          PYTHONPATH: roots.get(anchor.version),
        },
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
  const responseContracts = writePythonResponseContracts(temporary);
  const selectedVersion = process.argv[2];
  if (selectedVersion === '--request-contract-only') {
    const requestContracts = await writePythonRequestContracts(temporary);
    await exercisePythonRequestContracts(python, requestContracts);
  } else if (selectedVersion) {
    // Explicit CLI selection is a developer diagnostic only. The release npm
    // command supplies no argument and therefore cannot silently narrow the
    // configured matrix.
    await exerciseHistoricalMatrix(
      python,
      pip,
      temporary,
      responseContracts,
      selectedVersion,
    );
    process.exitCode = 0;
  } else {
    const requestContracts = await writePythonRequestContracts(temporary);
    await exercisePythonRequestContracts(python, requestContracts);
    await exercisePythonResponseContracts(python, responseContracts);
    await withScenarioServer('management', 'echo', PORT, async (baseURL) => {
      await runDriver(python, DRIVER, baseURL);
    });
    await withServer('worker', PORT + 1, async (baseURL) => {
      await runDriver(python, HELPER_DRIVER, baseURL);
    });
    await exerciseRecovery(python, temporary);
    await exerciseHistoricalMatrix(python, pip, temporary, responseContracts);
    const pinned = readFileSync(LOCK, 'utf8').match(/^anthropic==(\S+)$/mu)?.[1];
    console.log(`E2E PASS: official Python Managed SDK ${pinned} runtime compatibility.`);
  }
} finally {
  rmSync(temporary, { recursive: true, force: true });
}
