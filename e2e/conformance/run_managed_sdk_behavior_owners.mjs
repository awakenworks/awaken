// Execute every distinct real-process scenario that owns an official Managed
// SDK operation. The operation-to-owner relation is derived from the canonical
// generated SDK ledger by managed_ts_sdk_method_manifest.mjs; this runner owns
// no second resource list, method list, or package-script copy.

import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import {
  existsSync,
  mkdtempSync,
  readFileSync,
  realpathSync,
  rmSync,
} from 'node:fs';
import { tmpdir } from 'node:os';
import path, { resolve } from 'node:path';
import { extractOperationsFromPackageRoot } from '../../packages/managed-sdk-oracle/src/extract-operations.mjs';
import { officialBetaResourceProjection } from '../../packages/managed-sdk-oracle/src/conformance/resource-projection.mjs';
import {
  MANAGED_TS_METHOD_MANIFEST,
  managedTsMethodManifestForOperations,
} from './managed_ts_sdk_method_manifest.mjs';
import { assertOwnerOperationReceipts } from './managed_sdk_operation_receipts.mjs';

const E2E = resolve(import.meta.dirname, '..');
const TSX_CLI = resolve(E2E, 'node_modules/tsx/dist/cli.mjs');
const RECEIPT_HOOK = resolve(import.meta.dirname, 'managed_sdk_receipt_hook.mjs');
const PACKAGE_HOOK = resolve(import.meta.dirname, 'managed_sdk_package_hook.mjs');
const RECEIPT_NODE_OPTIONS = [
  process.env.NODE_OPTIONS,
  `--import=${PACKAGE_HOOK}`,
  `--import=${RECEIPT_HOOK}`,
]
  .filter(Boolean)
  .join(' ');
const candidateVersion = process.env.ANTHROPIC_SDK_CONFORMANCE_CANDIDATE_VERSION;
const configuredPackageRoot = process.env.ANTHROPIC_SDK_RUNTIME_PACKAGE_ROOT;
if (candidateVersion && !configuredPackageRoot) {
  throw new Error('candidate behavior owners require ANTHROPIC_SDK_RUNTIME_PACKAGE_ROOT');
}
const sdkRoot = realpathSync(
  configuredPackageRoot ?? resolve(E2E, 'node_modules/@anthropic-ai/sdk'),
);
const sdkManifest = JSON.parse(readFileSync(resolve(sdkRoot, 'package.json'), 'utf8'));
if (candidateVersion && sdkManifest.version !== candidateVersion) {
  throw new Error(
    `candidate behavior-owner root ${sdkManifest.version} does not match ${candidateVersion}`,
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
const behaviorManifest = managedTsMethodManifestForOperations(selectedOperations);
const skillsProjection = officialBetaResourceProjection(selectedOperations, 'skills').projection;
const owners = [...new Set(behaviorManifest.map(({ owner }) => owner))].sort();
const OWNER_TIMEOUT_MS = Number(process.env.AWAKEN_MANAGED_OWNER_TIMEOUT_MS ?? 180_000);
if (!Number.isSafeInteger(OWNER_TIMEOUT_MS) || OWNER_TIMEOUT_MS <= 0) {
  throw new Error('AWAKEN_MANAGED_OWNER_TIMEOUT_MS must be a positive safe integer');
}

async function executeOwner(owner, receiptFile, resolutionFile) {
  const args = owner.endsWith('.ts') ? [TSX_CLI, owner] : [owner];
  await new Promise((resolveOwner, rejectOwner) => {
    const child = spawn(process.execPath, args, {
      cwd: E2E,
      env: {
        ...process.env,
        AWAKEN_MANAGED_SDK_RECEIPT_FILE: receiptFile,
        AWAKEN_MANAGED_SDK_PACKAGE_ROOT: sdkRoot,
        AWAKEN_MANAGED_SDK_PACKAGE_VERSION: sdkVersion,
        AWAKEN_MANAGED_SDK_RESOLUTION_FILE: resolutionFile,
        AWAKEN_MANAGED_SDK_SKILLS_PROJECTION: skillsProjection,
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
const receiptDirectory = mkdtempSync(resolve(tmpdir(), 'awaken-managed-sdk-receipts-'));
let receiptCount = 0;
try {
  for (const [index, owner] of owners.entries()) {
    console.log(`[managed-sdk ${index + 1}/${owners.length}] ${owner}`);
    const receiptFile = resolve(receiptDirectory, `${index}.jsonl`);
    const resolutionFile = resolve(receiptDirectory, `${index}.resolution.jsonl`);
    await executeOwner(owner, receiptFile, resolutionFile);
    assertExactSdkResolution(owner, resolutionFile);
    receiptCount += assertOwnerOperationReceipts(
      behaviorManifest,
      owner,
      loadReceipts(receiptFile),
      sdkVersion,
    );
  }
} finally {
  rmSync(receiptDirectory, { recursive: true, force: true });
}

console.log(
  `Managed SDK ${sdkVersion} behavior owners PASS: ${receiptCount} HTTP operations + `
    + `${MANAGED_TS_METHOD_MANIFEST.length - receiptCount} helpers -> `
    + `${owners.length} real-process scenarios.`,
);
