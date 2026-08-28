// Execute every distinct real-process scenario that owns an official Managed
// SDK operation. The operation-to-owner relation is derived from the canonical
// generated SDK ledger by managed_ts_sdk_method_manifest.mjs; this runner owns
// no second resource list, method list, or package-script copy.

import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import {
  existsSync,
  linkSync,
  mkdtempSync,
  readFileSync,
  realpathSync,
  rmSync,
} from 'node:fs';
import { tmpdir } from 'node:os';
import path, { resolve } from 'node:path';
import {
  AWAKEN_BIN_ENV,
  SCENARIO_HOST_BIN_ENV,
  cargoExecutable,
  cargoScenarioHostBundle,
} from '../cargo_binary.mjs';
import { extractOperationsFromPackageRoot } from '../../packages/managed-sdk-oracle/src/extract-operations.mjs';
import { extractResponseContractsFromPackageRoot } from '../../packages/managed-sdk-oracle/src/extract-response-contracts.mjs';
import { resolveSdkPackage } from '../../packages/managed-sdk-oracle/src/package-source.mjs';
import { officialBetaResourceProjection } from '../../packages/managed-sdk-oracle/src/conformance/resource-projection.mjs';
import { managedTsMethodManifestForOperations } from './managed_ts_sdk_method_manifest.mjs';
import {
  MANAGED_SDK_RESPONSE_FINGERPRINT_KEY,
  assertOwnerOperationReceipts,
} from './managed_sdk_operation_receipts.mjs';
import { managedSdkOwnerProcessEnvironment } from './managed_sdk_process_environment.mjs';

const E2E = resolve(import.meta.dirname, '..');
const TSX_CLI = resolve(E2E, 'node_modules/tsx/dist/cli.mjs');
const RECEIPT_HOOK = resolve(import.meta.dirname, 'managed_sdk_receipt_hook.mjs');
const PACKAGE_HOOK = resolve(import.meta.dirname, 'managed_sdk_package_hook.mjs');
const BASE_OWNER_PROCESS_ENVIRONMENT = managedSdkOwnerProcessEnvironment(process.env, E2E);
const RECEIPT_NODE_OPTIONS = [
  process.env.NODE_OPTIONS,
  `--import=${PACKAGE_HOOK}`,
  `--import=${RECEIPT_HOOK}`,
]
  .filter(Boolean)
  .join(' ');
const candidateModule = process.env.ANTHROPIC_SDK_CONFORMANCE_CANDIDATE;
const candidateVersion = process.env.ANTHROPIC_SDK_CONFORMANCE_CANDIDATE_VERSION;
const configuredPackageRoot = process.env.ANTHROPIC_SDK_RUNTIME_PACKAGE_ROOT;
const expectedVersion = candidateVersion ?? process.env.AWAKEN_MANAGED_SDK_EXPECTED_VERSION;
const historicalMode = process.env.AWAKEN_MANAGED_SDK_HISTORICAL_SUBSET ?? '0';
assert.match(historicalMode, /^(?:0|1)$/u, 'historical SDK subset mode must be 0 or 1');
const historicalSubset = historicalMode === '1';
if (candidateModule || candidateVersion) {
  assert.equal(
    candidateModule,
    '@anthropic-ai/sdk-candidate',
    'candidate behavior owners require the reviewed exact package alias',
  );
  assert.match(
    candidateVersion ?? '',
    /^\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?$/u,
    'candidate behavior owners require one exact reviewed version',
  );
}
if (candidateVersion && !configuredPackageRoot) {
  throw new Error('candidate behavior owners require ANTHROPIC_SDK_RUNTIME_PACKAGE_ROOT');
}
if (candidateVersion && historicalSubset) {
  throw new Error('a candidate SDK cannot use the historical operation-subset rule');
}
if (configuredPackageRoot && !expectedVersion) {
  throw new Error('a configured behavior-owner root requires one exact expected SDK version');
}
const sdkRoot = realpathSync(
  configuredPackageRoot ?? resolve(E2E, 'node_modules/@anthropic-ai/sdk'),
);
const sdkManifest = JSON.parse(readFileSync(resolve(sdkRoot, 'package.json'), 'utf8'));
if (expectedVersion && sdkManifest.version !== expectedVersion) {
  throw new Error(
    `behavior-owner root ${sdkManifest.version} does not match ${expectedVersion}`,
  );
}
const sdkVersion = sdkManifest.version;
const scope = JSON.parse(readFileSync(resolve(
  E2E,
  '../packages/managed-sdk-oracle/config/scope.json',
), 'utf8'));
const extractedOperations = extractOperationsFromPackageRoot(sdkRoot, scope);
assert.equal(
  extractedOperations.version,
  sdkVersion,
  'selected operation source must belong to the selected SDK manifest',
);
const selectedOperations = extractedOperations.operations;
const responseContracts = extractResponseContractsFromPackageRoot(
  sdkRoot,
  scope,
  selectedOperations.map(({ id }) => id),
);

// Derive operation-local negative capability evidence from every admitted SDK,
// rather than maintaining a Files/Skills header list in the test harness. This
// matters at the 0.121 -> 0.122 change point: a caller-supplied legacy header
// must not make a query-only candidate request look generated-correct. Betas
// belonging to another operation family remain orthogonal and are allowed.
const anchorConfig = JSON.parse(readFileSync(resolve(
  E2E,
  '../packages/managed-sdk-oracle/config/anchors.json',
), 'utf8'));
const operationBetaUniverse = new Map();
for (const root of [
  ...anchorConfig.anchors.map(({ module }) => resolveSdkPackage(module).root),
  sdkRoot,
]) {
  for (const operation of extractOperationsFromPackageRoot(root, scope).operations) {
    const values = operationBetaUniverse.get(operation.id) ?? new Set();
    for (const beta of operation.betas) values.add(beta);
    operationBetaUniverse.set(operation.id, values);
  }
}
// Historical SDKs execute their own exact generated request code, but the API
// deliberately serves one canonical additive wire projection selected by the
// beta header—not a User-Agent/version-specific response. Validate those bytes
// strictly against the current oracle while retaining the historical contract
// above to prove that the operation's JSON/binary/stream media class did not
// change. This permits only fields and nullability reviewed into the canonical
// SDK; it does not turn historical validation into an open-object check.
let wireResponseContracts;
if (historicalSubset) {
  const oracle = JSON.parse(readFileSync(resolve(
    E2E,
    '../contracts/anthropic-managed/upstream-oracle.generated.json',
  ), 'utf8'));
  const canonicalRoot = resolveSdkPackage(oracle.current.module).root;
  wireResponseContracts = extractResponseContractsFromPackageRoot(
    canonicalRoot,
    scope,
    selectedOperations.map(({ id }) => id),
  );
}
const behaviorManifest = managedTsMethodManifestForOperations(selectedOperations, {
  allowHistoricalSubset: historicalSubset,
  responseContracts,
  wireResponseContracts,
}).map((operation) => {
  if (!operation.method) return operation;
  const selectedBetas = new Set(operation.betas);
  const forbiddenBetas = [...(operationBetaUniverse.get(operation.sdkMethod) ?? [])]
    .filter((beta) => !selectedBetas.has(beta))
    .sort();
  return Object.freeze({ ...operation, forbiddenBetas: Object.freeze(forbiddenBetas) });
});
const skillsProjection = officialBetaResourceProjection(selectedOperations, 'skills').projection;
const selectedOperationIDs = new Set(selectedOperations.map(({ id }) => id));
const owners = [...new Set(behaviorManifest.map(({ owner }) => owner))].sort();
const OWNER_TIMEOUT_MS = Number(process.env.AWAKEN_MANAGED_OWNER_TIMEOUT_MS ?? 180_000);
if (!Number.isSafeInteger(OWNER_TIMEOUT_MS) || OWNER_TIMEOUT_MS <= 0) {
  throw new Error('AWAKEN_MANAGED_OWNER_TIMEOUT_MS must be a positive safe integer');
}

// Build once outside every behavior timeout. A clean checkout or a contended
// shared cache may legitimately spend minutes compiling; neither is a hung SDK
// call. The resulting executables are hard-linked into an immutable suite
// snapshot so 20+ child processes never re-enter Cargo or observe replacement
// artifacts from another worktree. Cargo publishes a rebuilt executable by
// replacing its directory entry, so the snapshot retains the prebuild inode
// without duplicating hundreds of megabytes.
const repositoryRoot = resolve(E2E, '..');
const { scenarioHost, handCompanion } = cargoScenarioHostBundle({
  cwd: repositoryRoot,
  environment: BASE_OWNER_PROCESS_ENVIRONMENT,
  prebuiltEnvironmentName: SCENARIO_HOST_BIN_ENV,
});
const productionAwaken = cargoExecutable({
  cwd: repositoryRoot,
  packageName: 'awaken-cli',
  targetName: 'awaken',
  environment: BASE_OWNER_PROCESS_ENVIRONMENT,
  prebuiltEnvironmentName: AWAKEN_BIN_ENV,
});
async function executeOwner(owner, receiptFile, resolutionFile, ownerEnvironment) {
  const args = owner.endsWith('.ts') ? [TSX_CLI, owner] : [owner];
  await new Promise((resolveOwner, rejectOwner) => {
    const child = spawn(process.execPath, args, {
      cwd: E2E,
      env: {
        ...ownerEnvironment,
        AWAKEN_MANAGED_SDK_RECEIPT_FILE: receiptFile,
        AWAKEN_MANAGED_SDK_PACKAGE_ROOT: sdkRoot,
        AWAKEN_MANAGED_SDK_PACKAGE_VERSION: sdkVersion,
        AWAKEN_MANAGED_SDK_RESOLUTION_FILE: resolutionFile,
        AWAKEN_MANAGED_SDK_SKILLS_PROJECTION: skillsProjection,
        AWAKEN_MANAGED_SDK_HAS_GA_FILES: selectedOperationIDs.has('files.upload') ? '1' : '0',
        AWAKEN_MANAGED_SDK_HAS_GA_SKILLS: selectedOperationIDs.has('skills.create') ? '1' : '0',
        AWAKEN_MANAGED_SDK_HAS_DREAMS: selectedOperationIDs.has('beta.dreams.create') ? '1' : '0',
        AWAKEN_MANAGED_SDK_RESPONSE_FINGERPRINT_KEY: MANAGED_SDK_RESPONSE_FINGERPRINT_KEY,
        NODE_OPTIONS: RECEIPT_NODE_OPTIONS,
      },
      stdio: ['inherit', 'pipe', 'pipe'],
    });
    let output = '';
    let timedOut = false;
    const timeout = setTimeout(() => {
      timedOut = true;
      child.kill('SIGKILL');
    }, OWNER_TIMEOUT_MS);
    const tee = (chunk, destination) => {
      const text = chunk.toString();
      output += text;
      destination.write(text);
    };
    child.stdout.on('data', (chunk) => tee(chunk, process.stdout));
    child.stderr.on('data', (chunk) => tee(chunk, process.stderr));
    child.once('error', (error) => {
      clearTimeout(timeout);
      rejectOwner(error);
    });
    child.once('close', (status, signal) => {
      clearTimeout(timeout);
      if (timedOut) {
        rejectOwner(new Error(`${owner}: exceeded ${OWNER_TIMEOUT_MS}ms timeout`));
      } else if (signal) {
        rejectOwner(new Error(`${owner}: terminated by ${signal}`));
      } else if (status !== 0) {
        rejectOwner(new Error(`${owner}: exited with status ${status}`));
      } else if (/^\s*(?:E2E )?SKIP:/mu.test(output)) {
        rejectOwner(new Error(`${owner}: skipped and therefore supplied no compatibility evidence`));
      } else {
        resolveOwner();
      }
    });
  });
}

function loadReceipts(receiptFile) {
  return (existsSync(receiptFile) ? readFileSync(receiptFile, 'utf8') : '')
    .split('\n')
    .filter(Boolean)
    .map((line) => JSON.parse(line));
}

function assertExactSdkResolution(owner, resolutionFile) {
  const resolutions = loadReceipts(resolutionFile);
  assert.ok(resolutions.length > 0, `${owner}: did not import the official Managed SDK`);
  for (const resolution of resolutions) {
    assert.equal(resolution.version, sdkVersion, `${owner}: loaded an adjacent SDK version`);
    assert.match(resolution.specifier, /^@anthropic-ai\/sdk(?:\/|$)/u);
    const resolvedPath = realpathSync(new URL(resolution.url));
    const relative = path.relative(sdkRoot, resolvedPath);
    assert.ok(
      !relative.startsWith('..') && !path.isAbsolute(relative),
      `${owner}: SDK import escaped selected package root`,
    );
  }
}

// Causal coverage graph:
// C1 the current official SDK generates one canonical HTTP operation ledger;
// C2 the ownership projection binds every operation/helper to one scenario;
// C3 this runner executes every distinct owner in a fresh child process.
// C4 an official-SDK-only transport hook records every runtime HTTP edge;
// C5 package self-resolution and x-stainless-package-version both equal the
// selected exact SDK root/version.
// Effects: E1 no owner is skipped because a package script was hand-edited;
// E2 one owner shared by many methods executes once; E3 a signal, spawn error,
// non-zero result, capability skip, missing operation receipt, or adjacent SDK
// resolution fails the gate. Decision table: C1+C2+C3+C4+C5 => E1+E2;
// failed/skipped/misresolved child => E3. Static source invocation and exact
// SDK-surface checks run immediately before this gate; the receipts prove those
// attributed calls execute from the selected SDK rather than merely exist.
let receiptCount = 0;
const ownerFailures = [];
const executableDirectory = mkdtempSync(resolve(tmpdir(), 'awaken-managed-sdk-binaries-'));
try {
  const snapshotExecutable = (source) => {
    const destination = resolve(executableDirectory, path.basename(source));
    linkSync(source, destination);
    return destination;
  };
  const scenarioHostSnapshot = snapshotExecutable(scenarioHost);
  snapshotExecutable(handCompanion);
  const productionAwakenSnapshot = snapshotExecutable(productionAwaken);
  const ownerEnvironment = {
    ...BASE_OWNER_PROCESS_ENVIRONMENT,
    [AWAKEN_BIN_ENV]: productionAwakenSnapshot,
    [SCENARIO_HOST_BIN_ENV]: scenarioHostSnapshot,
  };
  const receiptDirectory = mkdtempSync(resolve(tmpdir(), 'awaken-managed-sdk-receipts-'));
  try {
    for (const [index, owner] of owners.entries()) {
      console.log(`[managed-sdk ${index + 1}/${owners.length}] ${owner}`);
      const receiptFile = resolve(receiptDirectory, `${index}.jsonl`);
      const resolutionFile = resolve(receiptDirectory, `${index}.resolution.jsonl`);
      try {
        await executeOwner(owner, receiptFile, resolutionFile, ownerEnvironment);
        assertExactSdkResolution(owner, resolutionFile);
        receiptCount += assertOwnerOperationReceipts(
          behaviorManifest,
          owner,
          loadReceipts(receiptFile),
          sdkVersion,
        );
      } catch (error) {
        ownerFailures.push(error);
      }
    }
  } finally {
    rmSync(receiptDirectory, { recursive: true, force: true });
  }
} finally {
  rmSync(executableDirectory, { recursive: true, force: true });
}

if (ownerFailures.length > 0) {
  throw new AggregateError(
    ownerFailures,
    `${ownerFailures.length} Managed SDK behavior owner(s) failed for ${sdkVersion}`,
  );
}

console.log(
  `Managed SDK ${sdkVersion} behavior owners PASS: ${receiptCount} HTTP operations + `
    + `${behaviorManifest.filter(({ method }) => !method).length} helpers -> `
    + `${owners.length} real-process scenarios.`,
);
